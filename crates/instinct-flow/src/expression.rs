//! Bounded evalexpr trees with typed admission and lazy absent-safe selection.
use crate::{Diagnostic, Limits, ValueType};
use evalexpr::{Node, Operator, Value};
use std::collections::BTreeMap;
pub type Values = BTreeMap<String, Value>;
#[derive(Debug, Clone)]
pub struct Expression {
    tree: Node,
    pub value_type: ValueType,
    source: String,
}
fn error(message: &str) -> Diagnostic {
    Diagnostic::new("expression", message, "expression")
}
pub fn identifier(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .enumerate()
            .all(|(i, c)| c.is_ascii_alphabetic() || c == b'_' || (i > 0 && c.is_ascii_digit()))
}
fn scan(text: &str, limits: &Limits) -> Result<(), Diagnostic> {
    if text.len() > limits.expression_bytes.min(4096) {
        return Err(error("expression exceeds byte limit"));
    }
    let mut chars = text.chars().peekable();
    let (mut tokens, mut operators, mut depth) = (0, 0, 0usize);
    while let Some(ch) = chars.next() {
        if ch.is_whitespace() {
            continue;
        }
        if ch == '/' && chars.peek() == Some(&'/') {
            chars.next();
            for c in chars.by_ref() {
                if c == '\n' {
                    break;
                }
            }
            continue;
        }
        if ch == '/' && chars.peek() == Some(&'*') {
            chars.next();
            let mut closed = false;
            while let Some(c) = chars.next() {
                if c == '*' && chars.peek() == Some(&'/') {
                    chars.next();
                    closed = true;
                    break;
                }
            }
            if !closed {
                return Err(error("unclosed comment"));
            }
            continue;
        }
        tokens += 1;
        match ch {
            '"' => {
                let mut closed = false;
                while let Some(c) = chars.next() {
                    match c {
                        '"' => {
                            closed = true;
                            break;
                        }
                        '\\' => match chars.next() {
                            Some('"' | '\\') => {}
                            _ => return Err(error("invalid escape")),
                        },
                        _ => {}
                    }
                }
                if !closed {
                    return Err(error("unclosed string"));
                }
            }
            '(' => {
                depth += 1;
                if depth > limits.expression_nesting.min(16) {
                    return Err(error("expression nesting limit"));
                }
            }
            ')' => {
                depth = depth
                    .checked_sub(1)
                    .ok_or_else(|| error("unmatched parenthesis"))?
            }
            '+' | '-' | '*' | '/' | '%' | '^' | '=' | '!' | '>' | '<' | '&' | '|' | ',' | ';' => {
                operators += 1;
                if chars.peek() == Some(&'=')
                    || (matches!(ch, '&' | '|') && chars.peek() == Some(&ch))
                {
                    chars.next();
                }
            }
            _ => {
                while chars.peek().is_some_and(|c| {
                    !c.is_whitespace()
                        && !matches!(
                            c,
                            '"' | '('
                                | ')'
                                | '+'
                                | '-'
                                | '*'
                                | '/'
                                | '%'
                                | '^'
                                | '='
                                | '!'
                                | '>'
                                | '<'
                                | '&'
                                | '|'
                                | ','
                                | ';'
                        )
                }) {
                    chars.next();
                }
            }
        }
        if tokens > limits.expression_tokens.min(128)
            || operators > limits.expression_operators.min(32)
        {
            return Err(error("expression complexity limit"));
        }
    }
    if depth != 0 {
        return Err(error("unclosed parenthesis"));
    }
    Ok(())
}
fn numeric(t: ValueType) -> bool {
    matches!(t, ValueType::Number | ValueType::Integer)
}
fn compatible(a: ValueType, b: ValueType) -> bool {
    a == b || (numeric(a) && numeric(b))
}
fn args(node: &Node) -> &[Node] {
    let mut node = node;
    while node.children().len() == 1
        && matches!(
            node.children()[0].operator(),
            Operator::RootNode | Operator::Tuple
        )
    {
        node = &node.children()[0];
    }
    node.children()
}
impl Expression {
    pub fn compile(text: &str, symbols: &BTreeMap<String, ValueType>) -> Result<Self, Diagnostic> {
        Self::compile_with_limits(text, symbols, &Limits::default())
    }
    pub fn compile_with_limits(
        text: &str,
        symbols: &BTreeMap<String, ValueType>,
        limits: &Limits,
    ) -> Result<Self, Diagnostic> {
        scan(text, limits)?;
        let tree =
            evalexpr::build_operator_tree(text).map_err(|_| error("invalid expression syntax"))?;
        let mut count = 0;
        fn infer(
            node: &Node,
            symbols: &BTreeMap<String, ValueType>,
            depth: usize,
            count: &mut usize,
        ) -> Result<ValueType, Diagnostic> {
            *count += 1;
            if depth > 32 || *count > 128 {
                return Err(error("expression AST limit"));
            }
            let infer_child =
                |child: &Node, count: &mut usize| infer(child, symbols, depth + 1, count);
            let children = node.children();
            match node.operator() {
                Operator::RootNode if children.len() == 1 => infer_child(&children[0], count),
                Operator::Const { value } => match value {
                    Value::String(_) => Ok(ValueType::String),
                    Value::Int(_) => Ok(ValueType::Integer),
                    Value::Float(n) if n.is_finite() => Ok(ValueType::Number),
                    Value::Boolean(_) => Ok(ValueType::Boolean),
                    _ => Err(error("unsupported literal")),
                },
                Operator::VariableIdentifierRead { identifier: id } if identifier(id) => symbols
                    .get(id)
                    .copied()
                    .ok_or_else(|| error("undeclared identifier")),
                Operator::FunctionIdentifier { identifier: id } => {
                    let a = args(node);
                    match id.as_str() {
                        "is_set" if a.len() == 1 => {
                            let mut arg = &a[0];
                            while matches!(arg.operator(), Operator::RootNode)
                                && arg.children().len() == 1
                            {
                                arg = &arg.children()[0];
                            }
                            if !matches!(arg.operator(),Operator::VariableIdentifierRead {identifier} if symbols.contains_key(identifier))
                            {
                                return Err(error("is_set requires a declared identifier"));
                            }
                            infer_child(arg, count)?;
                            Ok(ValueType::Boolean)
                        }
                        "if" if a.len() == 3 => {
                            let test = infer_child(&a[0], count)?;
                            let yes = infer_child(&a[1], count)?;
                            let no = infer_child(&a[2], count)?;
                            if test != ValueType::Boolean || !compatible(yes, no) {
                                return Err(error("if requires boolean and matching branch types"));
                            }
                            Ok(if numeric(yes) && numeric(no) && yes != no {
                                ValueType::Number
                            } else {
                                yes
                            })
                        }
                        _ => Err(error("function not allowed or wrong arity")),
                    }
                }
                op => {
                    let types = children
                        .iter()
                        .map(|c| infer_child(c, count))
                        .collect::<Result<Vec<_>, _>>()?;
                    match (op, types.as_slice()) {
                        (Operator::Neg, [a]) if numeric(*a) => Ok(*a),
                        (Operator::Not, [ValueType::Boolean]) => Ok(ValueType::Boolean),
                        (
                            Operator::And | Operator::Or,
                            [ValueType::Boolean, ValueType::Boolean],
                        ) => Ok(ValueType::Boolean),
                        (
                            Operator::Add
                            | Operator::Sub
                            | Operator::Mul
                            | Operator::Div
                            | Operator::Mod,
                            [a, b],
                        ) if numeric(*a) && numeric(*b) => {
                            Ok(if *a == ValueType::Integer && *b == ValueType::Integer {
                                ValueType::Integer
                            } else {
                                ValueType::Number
                            })
                        }
                        (Operator::Eq | Operator::Neq, [a, b])
                            if compatible(*a, *b)
                                && matches!(
                                    a,
                                    ValueType::String
                                        | ValueType::Boolean
                                        | ValueType::Number
                                        | ValueType::Integer
                                ) =>
                        {
                            Ok(ValueType::Boolean)
                        }
                        (Operator::Lt | Operator::Leq | Operator::Gt | Operator::Geq, [a, b])
                            if compatible(*a, *b) && (*a == ValueType::String || numeric(*a)) =>
                        {
                            Ok(ValueType::Boolean)
                        }
                        _ => Err(error("operator or operand type not allowed")),
                    }
                }
            }
        }
        let value_type = infer(&tree, symbols, 0, &mut count)?;
        fn depth(node: &Node) -> usize {
            1 + node.children().iter().map(depth).max().unwrap_or(0)
        }
        if count > limits.expression_ast_nodes.min(128)
            || depth(&tree) > limits.expression_ast_depth.min(32)
        {
            return Err(error("expression AST exceeds host ceiling"));
        }
        Ok(Self {
            tree,
            value_type,
            source: text.into(),
        })
    }
    pub(crate) fn check_limits(&self, limits: &Limits) -> Result<(), Diagnostic> {
        scan(&self.source, limits)?;
        fn measure(node: &Node) -> (usize, usize) {
            let children: Vec<_> = node.children().iter().map(measure).collect();
            (
                1 + children.iter().map(|(n, _)| n).sum::<usize>(),
                1 + children.iter().map(|(_, d)| *d).max().unwrap_or(0),
            )
        }
        let (nodes, depth) = measure(&self.tree);
        if nodes > limits.expression_ast_nodes.min(128)
            || depth > limits.expression_ast_depth.min(32)
        {
            return Err(error("expression AST exceeds host ceiling"));
        }
        Ok(())
    }
    pub fn evaluate(&self, values: &Values) -> Result<Value, Diagnostic> {
        fn boolean(v: Value) -> Result<bool, Diagnostic> {
            if let Value::Boolean(v) = v {
                Ok(v)
            } else {
                Err(error("boolean required; absent cannot be consumed"))
            }
        }
        fn ev(node: &Node, values: &Values) -> Result<Value, Diagnostic> {
            let c = node.children();
            match node.operator() {
                Operator::RootNode => ev(&c[0], values),
                Operator::Const { value } => Ok(value.clone()),
                Operator::VariableIdentifierRead { identifier } => {
                    Ok(values.get(identifier).cloned().unwrap_or(Value::Empty))
                }
                Operator::And => {
                    let left = boolean(ev(&c[0], values)?)?;
                    if !left {
                        Ok(Value::Boolean(false))
                    } else {
                        Ok(Value::Boolean(boolean(ev(&c[1], values)?)?))
                    }
                }
                Operator::Or => {
                    let left = boolean(ev(&c[0], values)?)?;
                    if left {
                        Ok(Value::Boolean(true))
                    } else {
                        Ok(Value::Boolean(boolean(ev(&c[1], values)?)?))
                    }
                }
                Operator::FunctionIdentifier { identifier } => {
                    let a = args(node);
                    if identifier == "is_set" {
                        Ok(Value::Boolean(!matches!(ev(&a[0], values)?, Value::Empty)))
                    } else {
                        let test = boolean(ev(&a[0], values)?)?;
                        ev(&a[if test { 1 } else { 2 }], values)
                    }
                }
                op => {
                    let a = ev(&c[0], values)?;
                    if a == Value::Empty {
                        return Err(error("absent operand"));
                    }
                    if *op == Operator::Not {
                        return Ok(Value::Boolean(!boolean(a)?));
                    }
                    if *op == Operator::Neg {
                        return match a {
                            Value::Int(n) => n
                                .checked_neg()
                                .map(Value::Int)
                                .ok_or_else(|| error("integer overflow")),
                            Value::Float(n) if n.is_finite() => Ok(Value::Float(-n)),
                            Value::Float(_) => Err(error("nonfinite arithmetic result")),
                            _ => Err(error("numeric operand required")),
                        };
                    }
                    let b = ev(&c[1], values)?;
                    if b == Value::Empty {
                        return Err(error("absent operand"));
                    }
                    fn num(v: &Value) -> Result<f64, Diagnostic> {
                        match v {
                            Value::Int(n) => Ok(*n as f64),
                            Value::Float(n) => Ok(*n),
                            _ => Err(error("numeric operand required")),
                        }
                    }
                    let order = || -> Result<std::cmp::Ordering, Diagnostic> {
                        fn mixed_order(n: i64, f: f64) -> Result<std::cmp::Ordering, Diagnostic> {
                            use std::cmp::Ordering;
                            if f.is_nan() {
                                return Err(error("nonfinite comparison"));
                            }
                            if f >= -(i64::MIN as f64) {
                                return Ok(Ordering::Less);
                            }
                            if f < i64::MIN as f64 {
                                return Ok(Ordering::Greater);
                            }
                            // The bounded truncation is exact; compare the fractional part only
                            // when the integer parts match, without rounding the integer to f64.
                            Ok(n.cmp(&(f as i64))
                                .then_with(|| 0.0.partial_cmp(&f.fract()).unwrap()))
                        }
                        match (&a, &b) {
                            (Value::String(a), Value::String(b)) => Ok(a.cmp(b)),
                            (Value::Boolean(a), Value::Boolean(b)) => Ok(a.cmp(b)),
                            (Value::Int(a), Value::Int(b)) => Ok(a.cmp(b)),
                            (Value::Int(a), Value::Float(b)) => mixed_order(*a, *b),
                            (Value::Float(a), Value::Int(b)) => mixed_order(*b, *a).map(|o| o.reverse()),
                            _ => num(&a)?
                                .partial_cmp(&num(&b)?)
                                .ok_or_else(|| error("nonfinite comparison")),
                        }
                    };
                    match op {
                        Operator::Eq => Ok(Value::Boolean(order()?.is_eq())),
                        Operator::Neq => Ok(Value::Boolean(!order()?.is_eq())),
                        Operator::Lt => Ok(Value::Boolean(order()?.is_lt())),
                        Operator::Leq => Ok(Value::Boolean(!order()?.is_gt())),
                        Operator::Gt => Ok(Value::Boolean(order()?.is_gt())),
                        Operator::Geq => Ok(Value::Boolean(!order()?.is_lt())),
                        _ => {
                            let (x, y) = (num(&a)?, num(&b)?);
                            if matches!(op, Operator::Div | Operator::Mod) && y == 0.0 {
                                return Err(error("division by zero"));
                            }
                            if let (Value::Int(x), Value::Int(y)) = (&a, &b) {
                                let n = match op {
                                    Operator::Add => x.checked_add(*y),
                                    Operator::Sub => x.checked_sub(*y),
                                    Operator::Mul => x.checked_mul(*y),
                                    Operator::Div => x.checked_div(*y),
                                    Operator::Mod => x.checked_rem(*y),
                                    _ => None,
                                }
                                .ok_or_else(|| error("integer overflow"))?;
                                return Ok(Value::Int(n));
                            }
                            let n = match op {
                                Operator::Add => x + y,
                                Operator::Sub => x - y,
                                Operator::Mul => x * y,
                                Operator::Div => x / y,
                                Operator::Mod => x % y,
                                _ => return Err(error("unsupported operator")),
                            };
                            if !n.is_finite() {
                                return Err(error("nonfinite arithmetic result"));
                            }
                            Ok(Value::Float(n))
                        }
                    }
                }
            }
        }
        ev(&self.tree, values)
    }
    pub fn boolean(&self, values: &Values) -> Result<bool, Diagnostic> {
        match self.evaluate(values)? {
            Value::Boolean(v) => Ok(v),
            _ => Err(error("condition must be boolean")),
        }
    }
}
pub fn from_json(value: Option<&serde_json::Value>) -> Value {
    match value {
        Some(serde_json::Value::String(s)) => Value::String(s.clone()),
        Some(serde_json::Value::Bool(v)) => Value::Boolean(*v),
        Some(serde_json::Value::Number(n)) => n
            .as_i64()
            .map(Value::Int)
            .or_else(|| n.as_f64().map(Value::Float))
            .unwrap_or(Value::Empty),
        Some(v) if !v.is_null() => Value::String(v.to_string()),
        _ => Value::Empty,
    }
}
pub fn to_json(value: Value) -> serde_json::Value {
    match value {
        Value::String(s) => s.into(),
        Value::Boolean(b) => b.into(),
        Value::Int(n) => n.into(),
        Value::Float(n) => serde_json::json!(n),
        _ => serde_json::Value::Null,
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn laziness_protects_absent_operands() {
        let symbols = BTreeMap::from([("x".into(), ValueType::Number)]);
        assert!(!Expression::compile("is_set(x) && x > 0.5", &symbols)
            .unwrap()
            .boolean(&Values::new())
            .unwrap());
        assert_eq!(
            Expression::compile("if(is_set(x), x, 0.0)", &symbols)
                .unwrap()
                .evaluate(&Values::new())
                .unwrap(),
            Value::Float(0.0)
        );
        assert!(Expression::compile("x + 1", &symbols)
            .unwrap()
            .evaluate(&Values::new())
            .is_err());
        assert!(Expression::compile("if(true, 1, \"bad\")", &symbols).is_err());
    }
    #[test]
    fn unsafe_operators_and_complexity_are_rejected() {
        let comparison_symbols = BTreeMap::from([
            ("i".into(), ValueType::Integer),
            ("f".into(), ValueType::Number),
        ]);
        for (integer, float, order) in [
            (9_007_199_254_740_993, 9_007_199_254_740_992.0, 1),
            (-9_007_199_254_740_993, -9_007_199_254_740_992.0, -1),
            (i64::MAX, -(i64::MIN as f64), -1),
            (i64::MIN, i64::MIN as f64, 0),
            (1, 1.5, -1),
            (-1, -1.5, 1),
            (0, 0.0, 0),
            (0, f64::INFINITY, -1),
            (0, f64::NEG_INFINITY, 1),
        ] {
            let values = Values::from([
                ("i".into(), Value::Int(integer)),
                ("f".into(), Value::Float(float)),
            ]);
            for (source, expected) in [
                ("i == f", order == 0),
                ("f == i", order == 0),
                ("i != f", order != 0),
                ("i < f", order < 0),
                ("f < i", order > 0),
                ("i <= f", order <= 0),
                ("f <= i", order >= 0),
                ("i > f", order > 0),
                ("f > i", order < 0),
                ("i >= f", order >= 0),
                ("f >= i", order <= 0),
            ] {
                assert_eq!(
                    Expression::compile(source, &comparison_symbols)
                        .unwrap()
                        .boolean(&values)
                        .unwrap(),
                    expected,
                    "{integer} {source} {float}"
                );
            }
        }
        let symbols = BTreeMap::from([("x".into(), ValueType::Number)]);
        let neg = Expression::compile("-x", &symbols).unwrap();
        for (operand, expected) in [
            (Value::Int(5), Ok(Value::Int(-5))),
            (Value::Float(1.5), Ok(Value::Float(-1.5))),
            (Value::Int(i64::MIN), Err("integer overflow")),
            (Value::Float(f64::INFINITY), Err("nonfinite arithmetic result")),
            (Value::Float(f64::NAN), Err("nonfinite arithmetic result")),
            (Value::Empty, Err("absent operand")),
            (Value::Boolean(true), Err("numeric operand required")),
        ] {
            assert_eq!(
                neg.evaluate(&Values::from([("x".into(), operand)])),
                expected.map_err(error)
            );
        }
        for source in [
            "1 ^ 2",
            "x = 2",
            "1; 2",
            "(1,2)",
            "sqrt(2)",
            "\"a\" + \"b\"",
            "is_set(1)",
            "a.b",
        ] {
            assert!(
                Expression::compile(source, &BTreeMap::new()).is_err(),
                "{source}"
            );
        }
        assert!(Expression::compile(
            &format!("{}1{}", "(".repeat(17), ")".repeat(17)),
            &BTreeMap::new()
        )
        .is_err());
        assert!(Expression::compile("1 / 0 == 2", &BTreeMap::new())
            .unwrap()
            .boolean(&Values::new())
            .is_err());
        assert!(Expression::compile("/* ^ = ; */ true", &BTreeMap::new())
            .unwrap()
            .boolean(&Values::new())
            .unwrap());
    }
}
