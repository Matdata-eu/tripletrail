//! Jena rule files to SHACL-AF SPARQL rules (#831): `tripletrail import-jena-rules`.
//!
//! Jena's `GenericRuleReasoner` reads rules such as
//!
//! ```text
//! @prefix ex: <http://example.org/> .
//! [transitive: (?a ex:partOf ?b), (?b ex:partOf ?c) -> (?a ex:partOf ?c)]
//! ```
//!
//! Each forward rule (`->`, and a backward rule `<-` whose head is triples, which infers the
//! same triples) becomes a node shape with one `sh:SPARQLRule`: the body's triple patterns
//! and builtins as the `WHERE`, the head's triples as the `CONSTRUCT`. A variable of the
//! body's first triple becomes `$this`, and the shape targets what it can be (`sh:targetClass`
//! for `rdf:type`, the subjects or objects of a predicate, or a SPARQL target). `tripletrail
//! infer` then runs the rules to their fixpoint, as the forward engine does.
//!
//! Builtins that have a SPARQL counterpart are mapped (comparisons, `noValue`, arithmetic,
//! `strConcat`, `regex`, `makeTemp`, `now`...). A rule with anything else (a functor, a
//! builtin in its head, nested backward rules, `table`, a builtin without a counterpart) is
//! left out and listed, with the reason.

use std::collections::HashMap;
use std::fmt::Write as _;

const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";

/// The SHACL-AF rules made from a rule file, and the rules left out.
#[derive(Debug)]
pub struct Translation {
    pub turtle: String,
    pub translated: usize,
    /// The name (or number) of each rule left out, and why.
    pub skipped: Vec<(String, String)>,
    /// The `@include`d rule sets (`<RDFS>`, other files), which are not read.
    pub includes: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
enum Node {
    Var(String),
    Iri(String),
    /// A literal, as SPARQL text.
    Literal(String),
    /// A functor `f(…)` inside a triple, which SPARQL has no form for.
    Functor(String),
}

#[derive(Clone, Debug)]
enum Clause {
    Triple(Node, Node, Node),
    Builtin(String, Vec<Node>),
    /// A nested rule `[ … ]` in a head.
    Nested,
}

struct Rule {
    name: String,
    body: Vec<Clause>,
    head: Vec<Clause>,
}

// ----- Reading -----

struct Lexer<'a> {
    text: &'a str,
    at: usize,
}

impl Lexer<'_> {
    fn skip_space(&mut self) {
        loop {
            let rest = &self.text[self.at..];
            let trimmed = rest.trim_start();
            self.at += rest.len() - trimmed.len();
            if trimmed.starts_with('#') || trimmed.starts_with("//") {
                self.at += trimmed.find('\n').unwrap_or(trimmed.len());
            } else {
                return;
            }
        }
    }

    fn peek(&mut self) -> Option<char> {
        self.skip_space();
        self.text[self.at..].chars().next()
    }

    fn eat(&mut self, s: &str) -> bool {
        self.skip_space();
        if self.text[self.at..].starts_with(s) {
            self.at += s.len();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, s: &str) -> Result<(), String> {
        if self.eat(s) {
            Ok(())
        } else {
            Err(self.error(&format!("expected {s}")))
        }
    }

    fn line(&self) -> usize {
        self.text[..self.at].matches('\n').count() + 1
    }

    fn error(&self, what: &str) -> String {
        let near: String = self.text[self.at..].chars().take(30).collect();
        format!("line {}: {what}, near {near:?}", self.line())
    }

    /// A run of name characters (a prefixed name, a variable name, a builtin name).
    fn word(&mut self) -> String {
        self.skip_space();
        let rest = &self.text[self.at..];
        let end = rest
            .find(|c: char| {
                !(c.is_alphanumeric() || matches!(c, '_' | '-' | ':' | '.' | '%' | '\u{b7}'))
            })
            .unwrap_or(rest.len());
        // A name does not end with `.`.
        let word = rest[..end].trim_end_matches('.');
        self.at += word.len();
        word.to_owned()
    }

    fn quoted(&mut self, quote: char) -> Result<String, String> {
        // At the opening quote.
        self.at += 1;
        let mut out = String::new();
        let mut chars = self.text[self.at..].char_indices();
        while let Some((i, c)) = chars.next() {
            match c {
                '\\' => {
                    let (_, e) = chars
                        .next()
                        .ok_or_else(|| self.error("a string ends in \\"))?;
                    out.push(match e {
                        'n' => '\n',
                        't' => '\t',
                        'r' => '\r',
                        e => e,
                    });
                }
                c if c == quote => {
                    self.at += i + 1;
                    return Ok(out);
                }
                c => out.push(c),
            }
        }
        Err(self.error("a string is not closed"))
    }
}

struct Reader<'a> {
    lex: Lexer<'a>,
    prefixes: HashMap<String, String>,
    includes: Vec<String>,
}

impl Reader<'_> {
    fn iri_of(&self, name: &str) -> Result<String, String> {
        let (prefix, local) = name
            .split_once(':')
            .ok_or_else(|| self.lex.error(&format!("{name} is not a prefixed name")))?;
        let ns = match self.prefixes.get(prefix) {
            Some(ns) => ns.clone(),
            None => match prefix {
                // Jena knows these without a declaration.
                "rdf" => "http://www.w3.org/1999/02/22-rdf-syntax-ns#".to_owned(),
                "rdfs" => "http://www.w3.org/2000/01/rdf-schema#".to_owned(),
                "owl" => "http://www.w3.org/2002/07/owl#".to_owned(),
                "xsd" => "http://www.w3.org/2001/XMLSchema#".to_owned(),
                _ => {
                    return Err(self
                        .lex
                        .error(&format!("the prefix {prefix}: is not declared")));
                }
            },
        };
        Ok(format!("{ns}{local}"))
    }

    fn node(&mut self) -> Result<Node, String> {
        match self.lex.peek() {
            Some('?') => {
                self.lex.at += 1;
                Ok(Node::Var(self.lex.word()))
            }
            Some('<') => {
                let rest = &self.lex.text[self.lex.at + 1..];
                let end = rest
                    .find('>')
                    .ok_or_else(|| self.lex.error("an IRI is not closed"))?;
                let iri = rest[..end].to_owned();
                self.lex.at += end + 2;
                Ok(Node::Iri(iri))
            }
            Some(q @ ('\'' | '"')) => {
                let lex = self.lex.quoted(q)?;
                let text = sparql_string(&lex);
                if self.lex.eat("^^") {
                    let dt = match self.lex.peek() {
                        Some('<') => match self.node()? {
                            Node::Iri(i) => i,
                            _ => unreachable!("an IRI"),
                        },
                        _ => {
                            let w = self.lex.word();
                            self.iri_of(&w)?
                        }
                    };
                    Ok(Node::Literal(format!("{text}^^<{dt}>")))
                } else if self.lex.text[self.lex.at..].starts_with('@') {
                    self.lex.at += 1;
                    let lang = self.lex.word();
                    Ok(Node::Literal(format!("{text}@{lang}")))
                } else {
                    Ok(Node::Literal(text))
                }
            }
            Some(c) if c.is_ascii_digit() || c == '-' || c == '+' => {
                let w = self.lex.word();
                if w.parse::<f64>().is_err() {
                    return Err(self.lex.error(&format!("{w} is not a number")));
                }
                Ok(Node::Literal(w))
            }
            Some(_) => {
                let w = self.lex.word();
                if w.is_empty() {
                    return Err(self.lex.error("expected a term"));
                }
                if self.lex.eat("(") {
                    // A functor: read and drop its arguments.
                    while !self.lex.eat(")") {
                        self.node()?;
                        self.lex.eat(",");
                    }
                    return Ok(Node::Functor(w));
                }
                if w == "true" || w == "false" {
                    return Ok(Node::Literal(w));
                }
                Ok(Node::Iri(self.iri_of(&w)?))
            }
            None => Err(self.lex.error("the file ends inside a rule")),
        }
    }

    fn clause(&mut self) -> Result<Clause, String> {
        if self.lex.eat("(") {
            let s = self.node()?;
            self.lex.eat(",");
            let p = self.node()?;
            self.lex.eat(",");
            let o = self.node()?;
            self.lex.expect(")")?;
            return Ok(Clause::Triple(s, p, o));
        }
        if self.lex.peek() == Some('[') {
            // A nested (backward) rule in a head: skipped whole.
            let mut depth = 0;
            for (i, c) in self.lex.text[self.lex.at..].char_indices() {
                match c {
                    '[' => depth += 1,
                    ']' => {
                        depth -= 1;
                        if depth == 0 {
                            self.lex.at += i + 1;
                            return Ok(Clause::Nested);
                        }
                    }
                    _ => {}
                }
            }
            return Err(self.lex.error("a nested rule is not closed"));
        }
        let name = self.lex.word();
        if name.is_empty() {
            return Err(self.lex.error("expected a triple or a builtin"));
        }
        self.lex.expect("(")?;
        let mut args = Vec::new();
        while !self.lex.eat(")") {
            args.push(self.node()?);
            self.lex.eat(",");
        }
        Ok(Clause::Builtin(name, args))
    }

    fn clauses(&mut self, until: &[&str]) -> Result<Vec<Clause>, String> {
        let mut out = Vec::new();
        loop {
            self.lex.skip_space();
            if until
                .iter()
                .any(|u| self.lex.text[self.lex.at..].starts_with(u))
            {
                return Ok(out);
            }
            out.push(self.clause()?);
            self.lex.eat(",");
        }
    }

    /// A rule, after its `[`.
    fn rule(&mut self, number: usize) -> Result<Rule, String> {
        // `name:` before the body, if there is one.
        let save = self.lex.at;
        let w = self.lex.word();
        let name = match w.strip_suffix(':') {
            Some(n) if !n.contains(':') && !n.is_empty() => n.to_owned(),
            _ => {
                self.lex.at = save;
                format!("rule {number}")
            }
        };
        let first = self.clauses(&["->", "<-"])?;
        if self.lex.eat("->") {
            let head = self.clauses(&["]"])?;
            self.lex.expect("]")?;
            Ok(Rule {
                name,
                body: first,
                head,
            })
        } else {
            self.lex.expect("<-")?;
            let body = self.clauses(&["]"])?;
            self.lex.expect("]")?;
            Ok(Rule {
                name,
                body,
                head: first,
            })
        }
    }
}

fn sparql_string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

type Read = (Vec<Rule>, HashMap<String, String>, Vec<String>);

fn read_rules(text: &str) -> Result<Read, String> {
    let mut r = Reader {
        lex: Lexer { text, at: 0 },
        prefixes: HashMap::new(),
        includes: Vec::new(),
    };
    let mut rules = Vec::new();
    loop {
        match r.lex.peek() {
            None => break,
            Some('@') => {
                if r.lex.eat("@prefix") {
                    let p = r.lex.word();
                    let p = p.strip_suffix(':').unwrap_or(&p).to_owned();
                    let Node::Iri(ns) = r.node()? else {
                        return Err(r.lex.error("@prefix takes an IRI"));
                    };
                    r.lex.eat(".");
                    r.prefixes.insert(p, ns);
                } else if r.lex.eat("@include") {
                    // `@include <RDFS>.` and the like: other rule files are not read.
                    let rest = &r.lex.text[r.lex.at..];
                    let end = rest.find('>').map_or(0, |e| e + 1);
                    r.includes.push(rest[..end].trim().to_owned());
                    r.node()?;
                    r.lex.eat(".");
                } else {
                    return Err(r.lex.error("unknown directive"));
                }
            }
            Some('[') => {
                r.lex.at += 1;
                let n = rules.len() + 1;
                rules.push(r.rule(n)?);
            }
            Some(_) => {
                // A rule without brackets: `body -> head .`
                let n = rules.len() + 1;
                let first = r.clauses(&["->", "<-"])?;
                let forward = r.lex.eat("->");
                if !forward {
                    r.lex.expect("<-")?;
                }
                let second = r.clauses(&["."])?;
                r.lex.expect(".")?;
                let (body, head) = if forward {
                    (first, second)
                } else {
                    (second, first)
                };
                rules.push(Rule {
                    name: format!("rule {n}"),
                    body,
                    head,
                });
            }
        }
    }
    Ok((rules, r.prefixes, r.includes))
}

// ----- Writing -----

fn sparql_node(n: &Node, this: &str) -> Result<String, String> {
    match n {
        Node::Var(v) if v == this => Ok("$this".to_owned()),
        Node::Var(v) => Ok(format!("?{v}")),
        Node::Iri(i) => Ok(format!("<{i}>")),
        Node::Literal(l) => Ok(l.clone()),
        Node::Functor(f) => Err(format!("the functor {f}(…) has no SPARQL form")),
    }
}

/// A builtin of the body as SPARQL (`FILTER …`, `BIND …`).
fn builtin(name: &str, args: &[Node], this: &str) -> Result<String, String> {
    let a = args
        .iter()
        .map(|n| sparql_node(n, this))
        .collect::<Result<Vec<_>, _>>()?;
    let n = a.len();
    let want = |k: usize| {
        if n == k {
            Ok(())
        } else {
            Err(format!("{name} takes {k} arguments, not {n}"))
        }
    };
    let compare = |op: &str| -> Result<String, String> {
        want(2)?;
        Ok(format!("FILTER({} {op} {})", a[0], a[1]))
    };
    let arith = |op: &str| -> Result<String, String> {
        want(3)?;
        Ok(format!("BIND({} {op} {} AS {})", a[0], a[1], a[2]))
    };
    Ok(match name {
        "equal" => compare("=")?,
        "notEqual" => compare("!=")?,
        "lessThan" => compare("<")?,
        "greaterThan" => compare(">")?,
        "le" => compare("<=")?,
        "ge" => compare(">=")?,
        "sameAs" => {
            want(2)?;
            format!("FILTER(sameTerm({}, {}))", a[0], a[1])
        }
        "isLiteral" => {
            want(1)?;
            format!("FILTER(isLiteral({}))", a[0])
        }
        "notLiteral" => {
            want(1)?;
            format!("FILTER(!isLiteral({}))", a[0])
        }
        "isBNode" => {
            want(1)?;
            format!("FILTER(isBlank({}))", a[0])
        }
        "notBNode" => {
            want(1)?;
            format!("FILTER(!isBlank({}))", a[0])
        }
        "isDType" => {
            want(2)?;
            format!("FILTER(datatype({}) = {})", a[0], a[1])
        }
        "notDType" => {
            want(2)?;
            format!(
                "FILTER(!isLiteral({0}) || datatype({0}) != {1})",
                a[0], a[1]
            )
        }
        "bound" => {
            want(1)?;
            format!("FILTER(bound({}))", a[0])
        }
        "unbound" => {
            want(1)?;
            format!("FILTER(!bound({}))", a[0])
        }
        "noValue" => match n {
            2 => format!("FILTER NOT EXISTS {{ {} {} ?_any }}", a[0], a[1]),
            3 => format!("FILTER NOT EXISTS {{ {} {} {} }}", a[0], a[1], a[2]),
            _ => return Err(format!("noValue takes 2 or 3 arguments, not {n}")),
        },
        "addOne" => {
            want(2)?;
            format!("BIND({} + 1 AS {})", a[0], a[1])
        }
        "sum" => arith("+")?,
        "difference" => arith("-")?,
        "product" => arith("*")?,
        "quotient" => arith("/")?,
        "min" | "max" => {
            want(3)?;
            let op = if name == "min" { "<" } else { ">" };
            format!("BIND(IF({0} {op} {1}, {0}, {1}) AS {2})", a[0], a[1], a[2])
        }
        "strConcat" | "uriConcat" => {
            if n < 2 {
                return Err(format!("{name} takes its parts and a result"));
            }
            let parts: Vec<String> = a[..n - 1].iter().map(|p| format!("STR({p})")).collect();
            let value = format!("CONCAT({})", parts.join(", "));
            let value = if name == "uriConcat" {
                format!("IRI({value})")
            } else {
                value
            };
            format!("BIND({value} AS {})", a[n - 1])
        }
        "regex" => match n {
            2 => format!("FILTER(regex(STR({}), {}))", a[0], a[1]),
            _ => return Err("regex with capture groups has no SPARQL form".to_owned()),
        },
        "strlen" => {
            want(2)?;
            format!("BIND(STRLEN(STR({})) AS {})", a[0], a[1])
        }
        "makeTemp" => {
            want(1)?;
            format!("BIND(BNODE() AS {})", a[0])
        }
        "now" => {
            want(1)?;
            format!("BIND(NOW() AS {})", a[0])
        }
        other => {
            return Err(format!(
                "the builtin {other} has no SPARQL counterpart here"
            ));
        }
    })
}

fn variables(n: &Node, out: &mut Vec<String>) {
    if let Node::Var(v) = n
        && !out.contains(v)
    {
        out.push(v.clone());
    }
}

/// The SHACL-AF shape of one rule, or why it can't be one.
fn shape(rule: &Rule, index: usize) -> Result<String, String> {
    let triples: Vec<(&Node, &Node, &Node)> = rule
        .body
        .iter()
        .filter_map(|c| match c {
            Clause::Triple(s, p, o) => Some((s, p, o)),
            _ => None,
        })
        .collect();
    let Some(&(s0, p0, o0)) = triples.first() else {
        return Err("the body has no triple to start from".to_owned());
    };
    // `$this` and the target, from the first triple of the body.
    let (this, target) = match (s0, p0, o0) {
        (Node::Var(s), Node::Iri(p), Node::Iri(c)) if p == RDF_TYPE => {
            (s.clone(), format!("sh:targetClass <{c}>"))
        }
        (Node::Var(s), Node::Iri(p), _) => (s.clone(), format!("sh:targetSubjectsOf <{p}>")),
        (_, Node::Iri(p), Node::Var(o)) => (o.clone(), format!("sh:targetObjectsOf <{p}>")),
        _ => {
            let mut vars = Vec::new();
            for n in [s0, p0, o0] {
                variables(n, &mut vars);
            }
            let v = vars
                .first()
                .ok_or("the first triple of the body has no variable")?
                .clone();
            let pattern = format!(
                "{} {} {}",
                sparql_node(s0, &v)?,
                sparql_node(p0, &v)?,
                sparql_node(o0, &v)?
            );
            let select = format!("SELECT DISTINCT $this WHERE {{ {pattern} }}");
            (
                v,
                format!(
                    "sh:target [ a sh:SPARQLTarget ; sh:select {} ]",
                    turtle_long_string(&select)
                ),
            )
        }
    };
    let mut body = Vec::new();
    for c in &rule.body {
        body.push(match c {
            Clause::Triple(s, p, o) => format!(
                "{} {} {} .",
                sparql_node(s, &this)?,
                sparql_node(p, &this)?,
                sparql_node(o, &this)?
            ),
            Clause::Builtin(name, args) => builtin(name, args, &this)?,
            Clause::Nested => return Err("a nested rule in the body".to_owned()),
        });
    }
    let mut head = Vec::new();
    for c in &rule.head {
        match c {
            Clause::Triple(s, p, o) => head.push(format!(
                "{} {} {} .",
                sparql_node(s, &this)?,
                sparql_node(p, &this)?,
                sparql_node(o, &this)?
            )),
            Clause::Builtin(name, _) => {
                return Err(format!("the head calls {name}, which a CONSTRUCT can't do"));
            }
            Clause::Nested => return Err("the head holds a backward rule".to_owned()),
        }
    }
    if head.is_empty() {
        return Err("the head has no triple".to_owned());
    }
    let construct = format!(
        "CONSTRUCT {{\n  {}\n}} WHERE {{\n  {}\n}}",
        head.join("\n  "),
        body.join("\n  ")
    );
    let iri = format!(
        "urn:x-jena-rule:{index}-{}",
        rule.name
            .chars()
            .map(|c| if c.is_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            })
            .collect::<String>()
    );
    Ok(format!(
        "<{iri}> a sh:NodeShape ;\n  rdfs:label {} ;\n  {target} ;\n  sh:rule [ a sh:SPARQLRule ; sh:construct {} ] .\n",
        sparql_string(&rule.name),
        turtle_long_string(&construct)
    ))
}

fn turtle_long_string(s: &str) -> String {
    format!(
        "\"\"\"{}\"\"\"",
        s.replace('\\', "\\\\").replace("\"\"\"", "\\\"\\\"\\\"")
    )
}

/// Translates a Jena rule file.
pub fn translate(text: &str) -> Result<Translation, String> {
    let (rules, prefixes, includes) = read_rules(text)?;
    let mut turtle = String::from(
        "@prefix sh: <http://www.w3.org/ns/shacl#> .\n\
         @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n",
    );
    let mut names: Vec<_> = prefixes.iter().collect();
    names.sort();
    for (p, ns) in names {
        if p != "sh" && p != "rdfs" {
            let _ = writeln!(turtle, "@prefix {p}: <{ns}> .");
        }
    }
    turtle.push('\n');
    let mut translated = 0;
    let mut skipped = Vec::new();
    for (i, rule) in rules.iter().enumerate() {
        match shape(rule, i + 1) {
            Ok(s) => {
                turtle.push_str(&s);
                turtle.push('\n');
                translated += 1;
            }
            Err(why) => skipped.push((rule.name.clone(), why)),
        }
    }
    if !skipped.is_empty() {
        turtle.push_str("# Rules left out:\n");
        for (name, why) in &skipped {
            let _ = writeln!(turtle, "#   {name}: {why}");
        }
    }
    for i in &includes {
        let _ = writeln!(turtle, "# Not read: @include {i}");
    }
    Ok(Translation {
        turtle,
        translated,
        skipped,
        includes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translates_forward_and_backward_rules_with_builtins() {
        let rules = r#"
# Comments are allowed.
@prefix ex: <http://example.org/> .
@include <RDFS>.
[transitive: (?a ex:partOf ?b), (?b ex:partOf ?c) -> (?a ex:partOf ?c)]
[typed: (?x rdf:type ex:Station), (?x ex:code ?c), strConcat('ST-', ?c, ?label)
    -> (?x rdfs:label ?label)]
[fast: (?s ex:maxSpeed ?v), greaterThan(?v, 160), noValue(?s ex:slow 'yes') -> (?s rdf:type ex:FastSection)]
[back: (?x ex:near ?y) <- (?y ex:near ?x)]
[skolem: (?a ex:p ?b) -> (?a ex:q f(?b))]
[printing: (?a ex:p ?b) -> print(?a)]
(?a ex:sibling ?b) -> (?b ex:sibling ?a) .
"#;
        let t = translate(rules).unwrap();
        assert_eq!(t.translated, 5, "{}", t.turtle);
        let names: Vec<&str> = t.skipped.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["skolem", "printing"]);
        assert!(
            t.turtle
                .contains("sh:targetSubjectsOf <http://example.org/partOf>")
        );
        assert!(
            t.turtle
                .contains("sh:targetClass <http://example.org/Station>")
        );
        assert!(
            t.turtle
                .contains("BIND(CONCAT(STR(\"ST-\"), STR(?c)) AS ?label)")
        );
        assert!(t.turtle.contains("FILTER($this > 160)") || t.turtle.contains("FILTER(?v > 160)"));
        assert!(
            t.turtle
                .contains("FILTER NOT EXISTS { $this <http://example.org/slow> \"yes\" }")
        );
        assert!(t.turtle.contains("functor f"));
        assert_eq!(t.includes, ["<RDFS>"]);
        // The Turtle reads.
        tripletrail::oxigraph::io::RdfParser::from_format(
            tripletrail::oxigraph::io::RdfFormat::Turtle,
        )
        .for_slice(t.turtle.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .unwrap_or_else(|e| panic!("{e}\n{}", t.turtle));
    }

    #[test]
    fn errors_name_the_line() {
        let e = translate("@prefix ex: <http://e/> .\n[r: (?a ex:p ?b) -> (?a nope:q ?b)]")
            .unwrap_err();
        assert!(e.starts_with("line 2:") && e.contains("nope"), "{e}");
        assert!(translate("[r: (?a <http://e/p> ?b) ->").is_err());
    }
}
