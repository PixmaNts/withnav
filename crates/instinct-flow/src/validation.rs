//! Graph, references, parameter types, and bounded port-choice safety checks.
use crate::{
    expression::{identifier, Expression},
    *,
};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) fn upstream(reference: &str) -> Option<&str> {
    reference
        .strip_prefix("answer.")
        .or_else(|| reference.strip_prefix("result."))
        .or_else(|| reference.strip_prefix("variable."))
        .and_then(|s| s.split('.').next())
}
pub(crate) fn reference_type(
    reference: &str,
    definition: &Definition,
    catalogue: &Catalogue,
    variables: &BTreeMap<String, ValueType>,
) -> Option<ValueType> {
    if let Some(name) = reference.strip_prefix("event.") {
        return catalogue
            .trigger(&definition.flow.on)?
            .payload_fields
            .iter()
            .find(|s| s.name == name)
            .map(|s| s.value_type);
    }
    if let Some(name) = reference.strip_prefix("constant.") {
        return scalar_type(definition.constants.get(name)?);
    }
    if let Some(name) = reference.strip_prefix("result.") {
        return definition
            .node
            .iter()
            .find(|n| n.id == name && matches!(n.kind, NodeKind::Expression { .. }))
            .map(|_| ValueType::Boolean);
    }
    if reference.starts_with("variable.") {
        return variables.get(reference).copied();
    }
    if let Some(tail) = reference.strip_prefix("answer.") {
        let mut parts = tail.split('.');
        let id = parts.next()?;
        let node = definition.node.iter().find(|n| n.id == id)?;
        match (parts.next(), &node.kind) {
            (None, NodeKind::Choice { .. }) => return Some(ValueType::String),
            (None, NodeKind::Noul { .. } | NodeKind::Score { .. }) => {
                return Some(ValueType::Number)
            }
            (Some("confidence"), NodeKind::Choice { .. } | NodeKind::Score { .. })
                if parts.next().is_none() =>
            {
                return Some(ValueType::Number)
            }
            (Some("probabilities"), NodeKind::Choice { criteria, .. }) => {
                let key = parts.next()?;
                if criteria.contains_key(key) && parts.next().is_none() {
                    return Some(ValueType::Number);
                }
            }
            (Some("probabilities"), NodeKind::Score { levels, .. }) => {
                let key = parts.next()?;
                if (0..levels.len()).any(|i| i.to_string() == key) && parts.next().is_none() {
                    return Some(ValueType::Number);
                }
            }
            _ => {}
        }
        return None;
    }
    catalogue
        .inputs
        .iter()
        .find(|s| s.name == reference && s.triggers.contains(&definition.flow.on))
        .map(|s| s.value_type)
}
fn scalar_type(v: &serde_json::Value) -> Option<ValueType> {
    match v {
        serde_json::Value::String(_) => Some(ValueType::String),
        serde_json::Value::Bool(_) => Some(ValueType::Boolean),
        serde_json::Value::Number(n) => Some(if n.is_i64() || n.is_u64() {
            ValueType::Integer
        } else {
            ValueType::Number
        }),
        _ => None,
    }
}
pub(crate) fn threshold(value: &Threshold, definition: &Definition) -> Option<f64> {
    match value {
        Threshold::Number(v) => Some(*v),
        Threshold::Reference(name) => definition
            .constants
            .get(name.strip_prefix("constant.")?)?
            .as_f64(),
    }
    .filter(|v| v.is_finite() && (0.0..=1.0).contains(v))
}
fn aliases(
    inputs: &BTreeMap<String, String>,
    definition: &Definition,
    catalogue: &Catalogue,
    variables: &BTreeMap<String, ValueType>,
    ancestors: Option<&BTreeSet<String>>,
    path: &str,
    errors: &mut Vec<Diagnostic>,
) -> BTreeMap<String, ValueType> {
    let mut types = BTreeMap::new();
    if inputs.len() > 16 {
        errors.push(Diagnostic::new(
            "limit",
            "more than 16 declared inputs",
            path,
        ));
    }
    for (alias, reference) in inputs {
        let field = format!("{path}.{alias}");
        if !identifier(alias) {
            errors.push(Diagnostic::new("schema", "invalid alias", &field));
        }
        if let Some(writer) = upstream(reference) {
            if !ancestors.is_some_and(|a| a.contains(writer)) {
                errors.push(Diagnostic::new(
                    "not_upstream",
                    "output is not a strict ancestor",
                    &field,
                ));
            }
        }
        if let Some(ty) = reference_type(reference, definition, catalogue, variables) {
            types.insert(alias.clone(), ty);
        } else {
            errors.push(Diagnostic::new(
                "unknown_input",
                "unknown reference",
                &field,
            ));
        }
    }
    types
}
fn action(
    action: &str,
    params: &BTreeMap<String, serde_json::Value>,
    types: &BTreeMap<String, ValueType>,
    definition: &Definition,
    catalogue: &Catalogue,
    path: &str,
    errors: &mut Vec<Diagnostic>,
) -> bool {
    let Some(spec) = catalogue.action(action) else {
        errors.push(Diagnostic::new("unknown_action", "unknown action", path));
        return false;
    };
    if !spec.allowed_triggers.contains(&definition.flow.on)
        || !catalogue
            .trigger(&definition.flow.on)
            .is_some_and(|s| s.allowed_actions.contains(&spec.name))
    {
        errors.push(Diagnostic::new(
            "action_not_allowed",
            "action is not allowed on this trigger",
            path,
        ));
    }
    if spec.kind == ActionKind::Verdict && !definition.flow.blocking {
        errors.push(Diagnostic::new(
            "action_not_allowed",
            "verdict requires blocking flow",
            path,
        ));
    }
    for param in &spec.params {
        if param.required && !params.contains_key(&param.name) {
            errors.push(Diagnostic::new(
                "invalid_parameter",
                "missing required parameter",
                format!("{path}.params.{}", param.name),
            ));
        }
    }
    for (name, value) in params {
        let field = format!("{path}.params.{name}");
        let Some(spec) = spec.params.iter().find(|p| p.name == *name) else {
            errors.push(Diagnostic::new(
                "invalid_parameter",
                "unknown parameter",
                field,
            ));
            continue;
        };
        if let Some(m) = value.as_object() {
            let ty = if m.len() == 1 {
                m.get("ref")
                    .and_then(|v| v.as_str())
                    .and_then(|name| types.get(name))
            } else {
                None
            };
            if ty != Some(&spec.value_type) {
                errors.push(Diagnostic::new(
                    "invalid_parameter",
                    "typed reference must name matching declared input",
                    &field,
                ));
            }
        } else if !spec.value_type.matches(value) || value.is_array() || value.is_null() {
            errors.push(Diagnostic::new(
                "invalid_parameter",
                "parameter must be matching scalar or typed reference",
                &field,
            ));
        }
        if let Some(text) = value.as_str() {
            if text.len() > 16384 {
                errors.push(Diagnostic::new(
                    "limit",
                    "action text exceeds byte limit",
                    &field,
                ));
            }
            match crate::rendering::placeholders(text, false) {
                Ok(names) => {
                    for name in names {
                        if !types.contains_key(&name) {
                            errors.push(Diagnostic::new(
                                "invalid_parameter",
                                "undeclared interpolation alias",
                                &field,
                            ));
                        }
                    }
                }
                Err(mut e) => {
                    e.path = field;
                    errors.push(e);
                }
            }
        }
    }
    spec.kind == ActionKind::Verdict
}
fn fallback_chain(
    chain: &FallbackAction,
    types: &BTreeMap<String, ValueType>,
    definition: &Definition,
    catalogue: &Catalogue,
    path: &str,
    errors: &mut Vec<Diagnostic>,
) -> usize {
    let (mut count, mut verdicts) = (0, 0);
    let mut current = Some(chain);
    while let Some(item) = current {
        count += 1;
        if count > 4 {
            errors.push(Diagnostic::new(
                "limit",
                "fallback chain exceeds depth four",
                path,
            ));
            break;
        }
        if action(
            &item.action,
            &item.params,
            types,
            definition,
            catalogue,
            path,
            errors,
        ) {
            verdicts += 1;
        }
        current = item.then.as_deref();
    }
    if verdicts > 1 {
        errors.push(Diagnostic::new(
            "conflicting_verdicts",
            "fallback has multiple verdicts",
            path,
        ));
    }
    count
}
pub(crate) fn validate(
    definition: &Definition,
    catalogue: &Catalogue,
    _capabilities: &Capabilities,
    templates: &BTreeMap<String, String>,
) -> Result<CompiledDefinition, Vec<Diagnostic>> {
    let mut errors = catalogue.validate().err().unwrap_or_default();
    let f = &definition.flow;
    if f.version != 1 {
        errors.push(Diagnostic::new(
            "schema",
            "unsupported flow version",
            "flow.version",
        ));
    }
    if f.name.is_empty()
        || f.name.len() > 64
        || !f
            .name
            .bytes()
            .enumerate()
            .all(|(i, c)| c.is_ascii_lowercase() || (i > 0 && (c.is_ascii_digit() || c == b'-')))
    {
        errors.push(Diagnostic::new("schema", "invalid flow name", "flow.name"));
    }
    let trigger = catalogue.trigger(&f.on);
    if trigger.is_none() {
        errors.push(Diagnostic::new(
            "unknown_trigger",
            "unknown trigger",
            "flow.on",
        ));
    }
    if f.blocking {
        if !trigger.is_some_and(|t| t.can_block) {
            errors.push(Diagnostic::new(
                "schema",
                "trigger cannot block",
                "flow.blocking",
            ));
        }
        if !f
            .budget_ms
            .is_some_and(|ms| ms > 0 && ms <= 2000 && ms <= catalogue.limits.blocking_budget_ms)
        {
            errors.push(Diagnostic::new(
                "limit",
                "blocking budget must be within host ceiling",
                "flow.budget_ms",
            ));
        }
    } else if f.budget_ms.is_some() {
        errors.push(Diagnostic::new(
            "schema",
            "budget_ms forbidden for non-blocking flow",
            "flow.budget_ms",
        ));
    }
    if definition.node.is_empty() || definition.node.len() > catalogue.limits.nodes.min(64) {
        errors.push(Diagnostic::new(
            "limit",
            "node count outside limits",
            "node",
        ));
    }
    if serde_json::to_vec(definition)
        .map_or(true, |v| v.len() > catalogue.limits.flow_bytes.min(262144))
    {
        errors.push(Diagnostic::new("limit", "flow exceeds byte limit", "flow"));
    }
    for (name, value) in &definition.constants {
        if !identifier(name) || scalar_type(value).is_none() {
            errors.push(Diagnostic::new(
                "schema",
                "constant must have valid name and scalar value",
                format!("constants.{name}"),
            ));
        }
    }
    let mut index = BTreeMap::new();
    for (i, node) in definition.node.iter().enumerate() {
        if !identifier(&node.id)
            || node.id == "trigger"
            || index.insert(node.id.clone(), i).is_some()
        {
            errors.push(
                Diagnostic::new(
                    "schema",
                    "invalid or duplicate node id",
                    format!("node[{}].id", node.id),
                )
                .at_node(&node.id),
            );
        }
        if node.label.as_ref().is_some_and(|s| s.len() > 128) {
            errors.push(Diagnostic::new(
                "limit",
                "label exceeds 128 bytes",
                format!("node[{}].label", node.id),
            ));
        }
    }
    let mut predecessors = vec![Vec::new(); definition.node.len()];
    let mut successors = vec![Vec::new(); definition.node.len()];
    let mut indegree = vec![0; definition.node.len()];
    for (i, node) in definition.node.iter().enumerate() {
        for (port, destinations) in &node.next {
            if !node.kind.ports().contains(port) {
                errors.push(Diagnostic::new(
                    "unknown_port",
                    "unknown output port",
                    format!("node[{}].next.{port}", node.id),
                ));
            }
            for dest in destinations {
                if let Some(&j) = index.get(dest) {
                    predecessors[j].push((i, port.clone()));
                    successors[i].push(j);
                    indegree[j] += 1;
                    if matches!(node.kind, NodeKind::Action { .. })
                        && !matches!(definition.node[j].kind, NodeKind::Action { .. })
                    {
                        errors.push(Diagnostic::new(
                            "schema",
                            "action then must target an action",
                            format!("node[{}].next", node.id),
                        ));
                    }
                } else {
                    errors.push(Diagnostic::new(
                        "unknown_node",
                        "wire targets unknown node",
                        format!("node[{}].next", node.id),
                    ));
                }
            }
        }
    }
    if f.start.is_empty() {
        errors.push(Diagnostic::new(
            "schema",
            "start must not be empty",
            "flow.start",
        ));
    }
    for start in &f.start {
        if !index.contains_key(start) {
            errors.push(Diagnostic::new(
                "unknown_node",
                "start targets unknown node",
                "flow.start",
            ));
        }
    }
    let mut order = Vec::new();
    let mut ready: BTreeSet<_> = indegree
        .iter()
        .enumerate()
        .filter_map(|(i, n)| (*n == 0).then_some(i))
        .collect();
    while let Some(i) = ready.pop_first() {
        order.push(i);
        for &j in &successors[i] {
            indegree[j] -= 1;
            if indegree[j] == 0 {
                ready.insert(j);
            }
        }
    }
    if order.len() != definition.node.len() {
        errors.push(Diagnostic::new("cycle", "graph contains a cycle", "node"));
    }
    let mut ancestors = vec![BTreeSet::new(); definition.node.len()];
    let mut reachable = vec![false; definition.node.len()];
    for &i in &order {
        reachable[i] = f.start.contains(&definition.node[i].id)
            || predecessors[i].iter().any(|(p, _)| reachable[*p]);
        if !reachable[i] {
            errors.push(Diagnostic::new(
                "unreachable",
                "node has no path from start",
                format!("node[{}]", definition.node[i].id),
            ));
        }
        for (p, _) in &predecessors[i] {
            let inherited = ancestors[*p].clone();
            ancestors[i].extend(inherited);
            ancestors[i].insert(definition.node[*p].id.clone());
        }
    }
    for (name, position) in &definition.layout {
        if (name != "trigger" && !index.contains_key(name))
            || !position.x.is_finite()
            || !position.y.is_finite()
        {
            errors.push(Diagnostic::new(
                "schema",
                "invalid layout node or coordinates",
                format!("layout.{name}"),
            ));
        }
    }
    let mut variables = BTreeMap::new();
    let mut expressions = BTreeMap::new();
    let flow_types = aliases(
        &f.inputs,
        definition,
        catalogue,
        &variables,
        None,
        "flow.inputs",
        &mut errors,
    );
    if let Some(when) = &f.when {
        compile(
            when,
            &flow_types,
            "flow.when",
            true,
            &mut expressions,
            &mut errors,
        );
    }
    let mut verdicts = vec![false; definition.node.len()];
    for &i in &order {
        let node = &definition.node[i];
        let path = format!("node[{}]", node.id);
        let mut types = aliases(
            &node.inputs,
            definition,
            catalogue,
            &variables,
            Some(&ancestors[i]),
            &format!("{path}.inputs"),
            &mut errors,
        );
        for reference in node.inputs.values() {
            if catalogue
                .inputs
                .iter()
                .any(|s| s.name == *reference && s.input_type == InputType::Image)
                && node.kind.question().is_some()
            {
                errors.push(Diagnostic::new(
                    "image_unsupported",
                    "image decisions are unsupported",
                    &path,
                ));
            }
        }
        if let Some(question) = node.kind.question() {
            if node.inputs.is_empty() && node.kind.template().is_none() {
                errors.push(
                    Diagnostic::new(
                        "schema",
                        "a decision needs at least one input: the instinct has nothing to judge",
                        format!("{path}.inputs"),
                    )
                    .at_node(&node.id),
                );
            }
            let instructions = match question {
                protocol::Question::Noul { instructions }
                | protocol::Question::Choice { instructions, .. }
                | protocol::Question::Score { instructions, .. } => instructions,
            };
            if instructions.len() > 4096 {
                errors.push(Diagnostic::new(
                    "limit",
                    "instructions exceed byte limit",
                    &path,
                ));
            }
        }
        match &node.kind {
            NodeKind::Noul { threshold: t, .. } => {
                if threshold(t, definition).is_none() {
                    errors.push(Diagnostic::new(
                        "schema",
                        "threshold must resolve to finite 0..1",
                        &path,
                    ));
                }
            }
            NodeKind::Choice { criteria, .. } => {
                if !(2..=16).contains(&criteria.len())
                    || criteria
                        .iter()
                        .any(|(k, v)| !identifier(k) || v.is_empty() || v.len() > 4096)
                {
                    errors.push(Diagnostic::new("schema", "invalid choice criteria", &path));
                }
            }
            NodeKind::Score {
                levels, at_least, ..
            } => {
                if !(2..=10).contains(&levels.len())
                    || levels.iter().any(|s| s.is_empty() || s.len() > 4096)
                    || levels.iter().collect::<BTreeSet<_>>().len() != levels.len()
                    || !levels.contains(at_least)
                {
                    errors.push(Diagnostic::new(
                        "schema",
                        "invalid ordered levels or threshold label",
                        &path,
                    ));
                }
            }
            NodeKind::Expression { condition, assign } => {
                if assign.len() > 16 {
                    errors.push(Diagnostic::new("limit", "more than 16 assignments", &path));
                }
                for (name, source) in assign {
                    if !identifier(name) || types.contains_key(name) {
                        errors.push(Diagnostic::new(
                            "expression",
                            "assignment cannot overwrite an input or earlier variable",
                            &path,
                        ));
                        continue;
                    }
                    let key = format!("{path}.assign.{name}");
                    if let Some(ty) =
                        compile(source, &types, &key, false, &mut expressions, &mut errors)
                    {
                        types.insert(name.clone(), ty);
                        variables.insert(format!("variable.{}.{name}", node.id), ty);
                    }
                }
                compile(
                    condition,
                    &types,
                    &format!("{path}.condition"),
                    true,
                    &mut expressions,
                    &mut errors,
                );
            }
            NodeKind::Action { action: a, params } => {
                verdicts[i] = action(a, params, &types, definition, catalogue, &path, &mut errors)
            }
        }
        if let Some(name) = node.kind.template() {
            if !template_name(name) {
                errors.push(Diagnostic::new(
                    "template",
                    "template must be a relative sibling filename",
                    &path,
                ));
                continue;
            }
            match templates.get(name) {
                None => errors.push(Diagnostic::new(
                    "template",
                    "template content missing",
                    &path,
                )),
                Some(text) if text.len() > catalogue.limits.template_bytes.min(65536) => errors
                    .push(Diagnostic::new(
                        "limit",
                        "template exceeds byte limit",
                        &path,
                    )),
                Some(text) => match crate::rendering::placeholders(text, true) {
                    Ok(names) => {
                        for name in names {
                            if !types.contains_key(&name) {
                                errors.push(Diagnostic::new(
                                    "template",
                                    "template uses undeclared alias",
                                    &path,
                                ));
                            }
                        }
                    }
                    Err(mut e) => {
                        e.path = path.clone();
                        errors.push(e);
                    }
                },
            }
        }
    }
    let mut fallback_types = BTreeMap::from([
        ("failure_kind".into(), ValueType::String),
        ("failure_message".into(), ValueType::String),
    ]);
    if let Some(fallback) = &definition.fallback {
        for name in fallback.inputs.keys() {
            if fallback_types.contains_key(name) {
                errors.push(Diagnostic::new(
                    "schema",
                    "reserved fallback alias",
                    format!("fallback.inputs.{name}"),
                ));
            }
        }
        fallback_types.extend(aliases(
            &fallback.inputs,
            definition,
            catalogue,
            &variables,
            None,
            "fallback.inputs",
            &mut errors,
        ));
        let mut count = 0;
        for (key, chain) in [
            ("on_failure", &fallback.on_failure),
            ("on_timeout", &fallback.on_timeout),
            ("on_provider_error", &fallback.on_provider_error),
            ("on_invalid_answer", &fallback.on_invalid_answer),
        ] {
            if let Some(chain) = chain {
                count += fallback_chain(
                    chain,
                    &fallback_types,
                    definition,
                    catalogue,
                    &format!("fallback.{key}"),
                    &mut errors,
                );
            }
        }
        if count > 16 {
            errors.push(Diagnostic::new(
                "limit",
                "more than 16 fallback actions",
                "fallback",
            ));
        }
    }
    if let Some(chain) = trigger
        .and_then(|t| t.default_fallback.as_ref())
        .filter(|_| f.blocking)
    {
        fallback_chain(
            chain,
            &fallback_types,
            definition,
            catalogue,
            "default_fallback",
            &mut errors,
        );
    }
    if errors.is_empty() {
        if let Err(e) = static_choices(
            definition,
            &order,
            &predecessors,
            &verdicts,
            catalogue.limits.decisions_per_run.min(16),
        ) {
            errors.push(e);
        }
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    for (path, expression) in &expressions {
        if let Err(mut error) = expression.check_limits(&catalogue.limits) {
            error.path = path.clone();
            errors.push(error);
        }
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    let bytes = serde_json::to_vec(&(definition, templates))
        .map_err(|e| vec![Diagnostic::new("schema", e.to_string(), "flow")])?;
    // SHA-256 is provided by the existing transitive ecosystem; explicit dependency keeps the digest stable.
    use sha2::{Digest, Sha256};
    let digest = format!("sha256:{:x}", Sha256::digest(bytes));
    Ok(CompiledDefinition {
        definition: definition.clone(),
        catalogue: catalogue.clone(),
        templates: templates.clone(),
        order,
        predecessors,
        expressions,
        digest,
    })
}
fn compile(
    text: &str,
    types: &BTreeMap<String, ValueType>,
    path: &str,
    boolean: bool,
    expressions: &mut BTreeMap<String, Expression>,
    errors: &mut Vec<Diagnostic>,
) -> Option<ValueType> {
    match Expression::compile(text, types) {
        Ok(e) if !boolean || e.value_type == ValueType::Boolean => {
            let ty = e.value_type;
            expressions.insert(path.into(), e);
            Some(ty)
        }
        Ok(_) => {
            errors.push(Diagnostic::new(
                "expression",
                "condition must have boolean type",
                path,
            ));
            None
        }
        Err(mut e) => {
            e.path = path.into();
            errors.push(e);
            None
        }
    }
}
pub fn template_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains(['/', '\\'])
        && !std::path::Path::new(name).is_absolute()
}
fn static_choices(
    definition: &Definition,
    order: &[usize],
    predecessors: &[Vec<(usize, String)>],
    verdicts: &[bool],
    limit: usize,
) -> Result<(), Diagnostic> {
    struct Search<'a> {
        d: &'a Definition,
        order: &'a [usize],
        preds: &'a [Vec<(usize, String)>],
        verdicts: &'a [bool],
        limit: usize,
        steps: usize,
        memo: BTreeSet<(usize, Vec<bool>, usize, usize)>,
    }
    impl Search<'_> {
        fn visit(
            &mut self,
            pos: usize,
            ports: &mut Vec<Option<String>>,
            decisions: usize,
            verdict_count: usize,
        ) -> Result<(), Diagnostic> {
            self.steps += 1;
            if self.steps > 100000 {
                return Err(Diagnostic::new(
                    "limit",
                    "static port-choice search budget exhausted",
                    "node",
                ));
            }
            if pos == self.order.len() {
                return Ok(());
            }
            let active_frontier = self.order[pos..]
                .iter()
                .map(|i| {
                    self.preds[*i]
                        .iter()
                        .any(|(p, port)| ports[*p].as_ref() == Some(port))
                })
                .collect();
            if !self
                .memo
                .insert((pos, active_frontier, decisions, verdict_count))
            {
                return Ok(());
            }
            let i = self.order[pos];
            let node = &self.d.node[i];
            let reached = self.d.flow.start.contains(&node.id)
                || self.preds[i]
                    .iter()
                    .any(|(p, port)| ports[*p].as_ref() == Some(port));
            if !reached {
                return self.visit(pos + 1, ports, decisions, verdict_count);
            }
            let decisions = decisions + usize::from(node.kind.question().is_some());
            let verdict_count = verdict_count + usize::from(self.verdicts[i]);
            if decisions > self.limit {
                return Err(Diagnostic::new(
                    "limit",
                    "more than allowed decisions reachable together",
                    "node",
                ));
            }
            if verdict_count > 1 {
                return Err(Diagnostic::new(
                    "conflicting_verdicts",
                    "multiple verdict actions reachable together",
                    "node",
                ));
            }
            for port in node.kind.ports() {
                ports[i] = Some(port);
                self.visit(pos + 1, ports, decisions, verdict_count)?;
            }
            ports[i] = None;
            Ok(())
        }
    }
    Search {
        d: definition,
        order,
        preds: predecessors,
        verdicts,
        limit,
        steps: 0,
        memo: BTreeSet::new(),
    }
    .visit(0, &mut vec![None; definition.node.len()], 0, 0)
}
