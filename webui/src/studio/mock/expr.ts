// A small evaluator of the flow expression language (contracts §2) for the mock's sample runs:
// numbers, strings, booleans, `+ - * / %`, comparisons, `! && ||`, `is_set(x)` and `if(c, a, b)`.
// Absent values are `undefined`; only `is_set` and `if` may consume them, as in the engine.

type Value = number | string | boolean | undefined;
type Token = { kind: "num" | "str" | "id" | "op"; text: string };

function tokenize(src: string): Token[] {
  const out: Token[] = [];
  let i = 0;
  while (i < src.length) {
    const c = src[i]!;
    if (/\s/.test(c)) i++;
    else if (/[0-9.]/.test(c)) {
      const m = /^[0-9]*\.?[0-9]+(?:[eE][-+]?[0-9]+)?/.exec(src.slice(i))!;
      out.push({ kind: "num", text: m[0] });
      i += m[0].length;
    } else if (c === '"' || c === "'") {
      const end = src.indexOf(c, i + 1);
      if (end < 0) throw new Error("unterminated string");
      out.push({ kind: "str", text: src.slice(i + 1, end) });
      i = end + 1;
    } else if (/[A-Za-z_]/.test(c)) {
      const m = /^[A-Za-z_][A-Za-z0-9_]*/.exec(src.slice(i))!;
      out.push({ kind: "id", text: m[0] });
      i += m[0].length;
    } else {
      const two = src.slice(i, i + 2);
      const op = ["==", "!=", "<=", ">=", "&&", "||"].includes(two) ? two : c;
      if (!"+-*/%<>!(),".includes(op[0]!) && op.length === 1) throw new Error(`unexpected "${c}"`);
      out.push({ kind: "op", text: op });
      i += op.length;
    }
  }
  return out;
}

export function evaluate(src: string, scope: Record<string, Value>): Value {
  const tokens = tokenize(src);
  let p = 0;
  const peek = () => tokens[p]?.text;
  const eat = (t: string) => {
    if (tokens[p]?.text !== t) throw new Error(`expected ${t}`);
    p++;
  };
  const need = (v: Value): Exclude<Value, undefined> => {
    if (v === undefined) throw new Error("absent value used");
    return v;
  };
  const or = (): Value => {
    let l = and();
    while (peek() === "||") {
      p++;
      const r = and();
      l = need(l) || need(r);
    }
    return l;
  };
  const and = (): Value => {
    let l = cmp();
    while (peek() === "&&") {
      p++;
      const r = cmp();
      l = need(l) && need(r);
    }
    return l;
  };
  const cmp = (): Value => {
    const l = add();
    const op = peek();
    if (op && ["==", "!=", "<", "<=", ">", ">="].includes(op)) {
      p++;
      const a = need(l);
      const b = need(add());
      switch (op) {
        case "==": return a === b;
        case "!=": return a !== b;
        case "<": return a < b;
        case "<=": return a <= b;
        case ">": return a > b;
        default: return a >= b;
      }
    }
    return l;
  };
  const add = (): Value => {
    let l = mul();
    while (peek() === "+" || peek() === "-") {
      const op = tokens[p++]!.text;
      const r = Number(need(mul()));
      l = op === "+" ? Number(need(l)) + r : Number(need(l)) - r;
    }
    return l;
  };
  const mul = (): Value => {
    let l = unary();
    while (peek() === "*" || peek() === "/" || peek() === "%") {
      const op = tokens[p++]!.text;
      const r = Number(need(unary()));
      const a = Number(need(l));
      if (op !== "*" && r === 0) throw new Error("division by zero");
      l = op === "*" ? a * r : op === "/" ? a / r : a % r;
    }
    return l;
  };
  const unary = (): Value => {
    if (peek() === "!") {
      p++;
      return !need(unary());
    }
    if (peek() === "-") {
      p++;
      return -Number(need(unary()));
    }
    return atom();
  };
  const atom = (): Value => {
    const t = tokens[p++];
    if (!t) throw new Error("unexpected end");
    if (t.kind === "num") return Number(t.text);
    if (t.kind === "str") return t.text;
    if (t.text === "(") {
      const v = or();
      eat(")");
      return v;
    }
    if (t.kind === "id") {
      if (t.text === "true") return true;
      if (t.text === "false") return false;
      if (t.text === "is_set") {
        eat("(");
        const name = tokens[p++]?.text ?? "";
        eat(")");
        return scope[name] !== undefined;
      }
      if (t.text === "if") {
        eat("(");
        const c = need(or());
        eat(",");
        const a = or();
        eat(",");
        const b = or();
        eat(")");
        return c ? a : b;
      }
      if (!(t.text in scope)) throw new Error(`unknown name ${t.text}`);
      return scope[t.text];
    }
    throw new Error(`unexpected "${t.text}"`);
  };
  const v = or();
  if (p !== tokens.length) throw new Error(`unexpected "${peek()}"`);
  return v;
}
