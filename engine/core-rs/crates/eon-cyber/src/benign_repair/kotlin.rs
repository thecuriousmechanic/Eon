//! Bounded K2 rules for the closed source-only benchmark, not a Kotlin resolver.

use super::Edit;
use serde_json::Value;
use tree_sitter::{Node, Parser, Tree};

const PRIMITIVES: [&str; 8] = [
    "Boolean", "Byte", "Short", "Int", "Long", "Float", "Double", "Char",
];

fn one<T>(mut iter: impl Iterator<Item = T>) -> Option<T> {
    let item = iter.next()?;
    iter.next().is_none().then_some(item)
}

fn child<'t>(n: Node<'t>, kind: &str) -> Option<Node<'t>> {
    one(n.children(&mut n.walk()).filter(|c| c.kind() == kind))
}

fn parent<'t>(n: Node<'t>, kind: &str) -> Option<Node<'t>> {
    n.parent().filter(|p| p.kind() == kind)
}

fn enclosing<'t>(n: Node<'t>, kind: &str) -> Option<Node<'t>> {
    std::iter::successors(Some(n), |n| n.parent()).find(|n| n.kind() == kind)
}

// Exact concrete children (including punctuation), ignoring only grammar extras.
fn shape<'t, const N: usize>(n: Node<'t>, kinds: [&str; N]) -> Option<[Node<'t>; N]> {
    let mut walk = n.walk();
    let mut children = n.children(&mut walk).filter(|c| !c.is_extra());
    let mut nodes = [n; N];
    for (out, kind) in nodes.iter_mut().zip(kinds) {
        *out = children.next()?;
        if kind != "_" && out.kind() != kind {
            return None;
        }
    }
    children.next().is_none().then_some(nodes)
}

fn descendants(root: Node<'_>) -> impl Iterator<Item = Node<'_>> {
    let mut stack = vec![root];
    std::iter::from_fn(move || {
        let n = stack.pop()?;
        stack.extend(n.named_children(&mut n.walk()));
        Some(n)
    })
}

// Only an existing horizontal gap after a complete member; never split a
// token, comment or literal, or create a closing brace missing in the source.
fn space_before_close(close: Node<'_>, bytes: &[u8]) -> Option<usize> {
    if close.kind() != "}" || bytes.get(close.byte_range())? != b"}" {
        return None;
    }
    let member = close.prev_sibling()?;
    if !member.is_named() || member.is_extra() || member.has_error() {
        return None;
    }
    // In particular, never terminate a line comment at the end of a member.
    let mut tail = member;
    while let Some(i) = tail.child_count().checked_sub(1) {
        tail = tail.child(i)?;
    }
    let gap = bytes.get(member.end_byte()..close.start_byte())?;
    (!tail.is_extra() && !gap.is_empty() && gap.iter().all(|b| matches!(b, b' ' | b'\t')))
        .then(|| close.start_byte() - 1)
}

pub(super) fn parse(parser: &mut Parser, src: &str, code: &str) -> Option<Tree> {
    let bytes = src.as_bytes();
    let mut tree = parser.parse(bytes, None)?;
    if !tree.root_node().has_error() {
        return Some(tree);
    }
    if code != "SUPERTYPE_NOT_INITIALIZED" {
        return None;
    }
    // kotlin-ng requires a member terminator even immediately before `}`.
    // Recovery may hide later classes. Probe one concrete brace per pass;
    // an error tree suggests a location but never authorizes a repair.
    let mut view = bytes.to_vec();
    let mut sites = Vec::new();
    for _ in 0..8 {
        let at = descendants(tree.root_node())
            .filter(|n| n.has_error() && (n.is_error() || n.kind() == "class_body"))
            .find_map(|n| {
                n.children(&mut n.walk())
                    .find_map(|c| space_before_close(c, &view))
            })?;
        view[at] = b'\n'; // one ASCII byte for one: all byte ranges map identically
        sites.push(at);
        tree = parser.parse(&view, None)?;
        let root = tree.root_node();
        if !root.has_error() {
            // Re-prove *every* probe in the full clean tree against the
            // original bytes: the newline can only separate a member from
            // the end of its class body, where Kotlin needs no terminator.
            let checked = sites.iter().all(|&i| {
                root.descendant_for_byte_range(i + 1, i + 2)
                    .is_some_and(|c| {
                        parent(c, "class_body").is_some_and(|body| body.end_byte() == c.end_byte())
                            && space_before_close(c, bytes) == Some(i)
                    })
            });
            return checked.then_some(tree);
        }
    }
    None
}

fn declaration_name(n: Node<'_>) -> Option<Node<'_>> {
    match n.kind() {
        "function_declaration"
        | "class_declaration"
        | "object_declaration"
        | "companion_object" => n.child_by_field_name("name"),
        "type_alias" => n.child_by_field_name("type"),
        // An untyped set(name) binding is a direct child, not a parameter node.
        "setter" => child(n, "identifier"),
        "parameter" | "class_parameter" | "variable_declaration" | "type_parameter" => n
            .named_children(&mut n.walk())
            .find(|c| c.kind() == "identifier"),
        _ => None,
    }
}

fn return_type(body: Node<'_>) -> Option<Node<'_>> {
    let fun = parent(body, "function_declaration")?;
    fun.children(&mut fun.walk())
        .filter(|n| !n.is_extra())
        .skip_while(|n| n.kind() != ":")
        .nth(1)
}

struct Subject<'t, 's> {
    root: Node<'t>,
    src: &'s str,
}

impl<'t, 's> Subject<'t, 's> {
    fn text(&self, n: Node<'_>) -> &'s str {
        &self.src[n.byte_range()]
    }

    fn declares(&self, n: Node<'_>, name: &str) -> bool {
        declaration_name(n)
            .is_some_and(|id| self.text(id).trim_matches('`') == name.trim_matches('`'))
    }

    fn resolve(&self, scope: Node<'t>, name: &str, kind: &str) -> Option<Node<'t>> {
        one(descendants(scope).filter(|&n| self.declares(n, name))).filter(|n| n.kind() == kind)
    }

    fn unshadowed(&self, name: &str) -> bool {
        !descendants(self.root).any(|n| self.declares(n, name))
    }

    fn simple_type(&self, ty: Node<'_>) -> Option<&'s str> {
        let [id] = shape(ty, ["identifier"])?;
        (ty.kind() == "user_type").then(|| self.text(id))
    }

    // Only parameters of the immediately enclosing named function. Don't infer
    // types through local bindings, nested functions or implicit lambda scopes.
    fn parameter_type(&self, use_: Node<'t>) -> Option<Node<'t>> {
        let fun = std::iter::successors(use_.parent(), |n| n.parent()).find(|n| {
            matches!(
                n.kind(),
                "function_declaration" | "lambda_literal" | "anonymous_function"
            )
        })?;
        let param = self.resolve(fun, self.text(use_), "parameter")?;
        (fun.kind() == "function_declaration"
            && param.parent()? == child(fun, "function_value_parameters")?)
        .then_some(shape(param, ["identifier", ":", "_"])?[2])
    }

    fn argument(&self, id: Node<'t>, msg: &str) -> Option<Edit> {
        let arg = parent(id, "value_argument")?;
        shape(arg, ["identifier"])?;
        let args = parent(arg, "value_arguments")?;
        shape(args, ["(", "value_argument", ")"])?;
        let [callee, _] = shape(
            parent(args, "call_expression")?,
            ["identifier", "value_arguments"],
        )?;
        let fun = self.resolve(self.root, self.text(callee), "function_declaration")?;
        // A bare call to a unique file-local non-extension function only.
        if fun.parent()? != self.root
            || child(fun, ".").is_some()
            || child(fun, "type_parameters").is_some()
        {
            return None;
        }
        let params = child(fun, "function_value_parameters")?;
        let param = child(params, "parameter")?;
        let expected = self.simple_type(shape(param, ["identifier", ":", "user_type"])?[2])?;
        let actual = self.simple_type(self.parameter_type(id)?)?;
        if !msg.ends_with(&format!(
            "actual type is '{actual}', but '{expected}' was expected."
        )) || !self.unshadowed(actual)
            || !self.unshadowed(expected)
        {
            return None;
        }
        if PRIMITIVES.contains(&expected) && actual.strip_suffix("Array") == Some(expected) {
            let [_, mods, _, _] = shape(params, ["(", "parameter_modifiers", "parameter", ")"])?;
            shape(shape(mods, ["parameter_modifier"])?[0], ["vararg"])?;
            return Some(insert(id.start_byte(), "*"));
        }
        shape(params, ["(", "parameter", ")"])?;
        (matches!(actual, "Byte" | "Short" | "Int") && expected == "Long")
            .then(|| insert(id.end_byte(), ".toLong()"))
    }

    fn safe_call(&self, dot: Node<'t>, msg: &str) -> Option<Edit> {
        let nav = parent(dot, "navigation_expression")?;
        let [receiver, _, _] = shape(nav, ["identifier", ".", "identifier"])?;
        let ty = self.parameter_type(receiver)?;
        let [inner, _] = shape(ty, ["user_type", "?"])?;
        let nullable = self.simple_type(inner)?;
        if ty.kind() != "nullable_type"
            || !msg.ends_with(&format!("nullable receiver of type '{nullable}?'."))
        {
            return None;
        }
        let expr = parent(nav, "call_expression").unwrap_or(nav);
        let body = parent(expr, "function_body")?;
        shape(body, ["=", "_"])?;
        (return_type(body)?.kind() == "nullable_type").then(|| (dot.byte_range(), "?.".into()))
    }

    fn const_val(&self, leaf: Node<'t>) -> Option<Edit> {
        let prop = enclosing(leaf, "property_declaration")?;
        let [mods, val, _, _, init] = shape(
            prop,
            [
                "modifiers",
                "val",
                "variable_declaration",
                "=",
                "call_expression",
            ],
        )?;
        let [modifier] = shape(mods, ["property_modifier"])?;
        let [kw] = shape(modifier, ["const"])?;
        if prop.parent()? != self.root || init.start_byte() != leaf.start_byte() {
            return None;
        }
        let gap = &self.src[kw.end_byte()..val.start_byte()];
        let end = if gap.bytes().all(|b| matches!(b, b' ' | b'\t')) {
            val.start_byte()
        } else {
            kw.end_byte()
        };
        Some((kw.start_byte()..end, String::new()))
    }

    fn supertype(&self, id: Node<'t>) -> Option<Edit> {
        let ty = parent(id, "user_type")?;
        let name = self.simple_type(ty)?;
        let spec = parent(ty, "delegation_specifier")?;
        shape(spec, ["user_type"])?;
        let specs = parent(spec, "delegation_specifiers")?;
        shape(specs, ["delegation_specifier"])?;
        let derived = parent(specs, "class_declaration")?;
        let base = self.resolve(self.root, name, "class_declaration")?;
        if derived == base || child(base, "primary_constructor").is_some() {
            return None;
        }
        for class in [derived, base] {
            child(class, "class")?;
            if class.parent()? != self.root
                || child(class, "enum_class_body").is_some()
                || child(class, "class_body").is_some_and(|body| {
                    body.named_children(&mut body.walk())
                        .any(|n| n.kind() == "secondary_constructor")
                })
            {
                return None;
            }
        }
        shape(
            child(child(base, "modifiers")?, "inheritance_modifier")?,
            ["open"],
        )?;
        Some(insert(ty.end_byte(), "()"))
    }

    fn operator(&self, receiver: Node<'t>, msg: &str) -> Option<Edit> {
        let index = parent(receiver, "index_expression")?;
        let [obj, _, _, _] = shape(index, ["identifier", "[", "_", "]"])?;
        if obj != receiver {
            return None;
        }
        let assignment =
            parent(index, "assignment").filter(|a| a.child_by_field_name("left") == Some(index));
        if assignment.is_some_and(|a| {
            a.child_by_field_name("operator")
                .is_none_or(|op| op.kind() != "=")
        }) || parent(index, "unary_expression").is_some()
        {
            return None;
        }
        let set = assignment.is_some();
        let name = if set { "set" } else { "get" };
        let owner = self.simple_type(self.parameter_type(obj)?)?;
        if !msg.starts_with(&format!("'operator' modifier is required on 'fun {name}("))
            || !msg.ends_with(&format!("' defined in '{owner}'."))
        {
            return None;
        }
        let class = self.resolve(self.root, owner, "class_declaration")?;
        let body = child(class, "class_body")?;
        let method = self.resolve(body, name, "function_declaration")?;
        if method.parent()? != body
            || method.child(0)?.kind() != "fun"
            || child(method, ".").is_some()
        {
            return None;
        }
        let params = child(method, "function_value_parameters")?;
        if set {
            shape(params, ["(", "parameter", ",", "parameter", ")"])?;
        } else {
            shape(params, ["(", "parameter", ")"])?;
        }
        Some(insert(method.start_byte(), "operator "))
    }

    fn data_parameter(&self, id: Node<'t>) -> Option<Edit> {
        let param = parent(id, "class_parameter")?;
        let [name, _, _] = shape(param, ["identifier", ":", "_"])?;
        let params = parent(param, "class_parameters")?;
        let class = parent(parent(params, "primary_constructor")?, "class_declaration")?;
        shape(
            child(child(class, "modifiers")?, "class_modifier")?,
            ["data"],
        )?;
        if child(class, "class_body").is_some_and(|body| {
            body.named_children(&mut body.walk())
                .filter(|member| member.kind() == "property_declaration")
                .filter_map(|member| child(member, "variable_declaration"))
                .any(|binding| self.declares(binding, self.text(name)))
        }) {
            return None;
        }
        (id == name && self.resolve(params, self.text(id), "class_parameter")? == param)
            .then(|| insert(name.start_byte(), "val "))
    }

    fn lazy_val(&self, by: Node<'t>, msg: &str) -> Option<Edit> {
        let delegate = parent(by, "property_delegate")?;
        let [_, call] = shape(delegate, ["by", "call_expression"])?;
        let [lazy, _] = shape(call, ["identifier", "annotated_lambda"])?;
        let prop = parent(delegate, "property_declaration")?;
        let [var, binding, _] = shape(prop, ["var", "variable_declaration", "property_delegate"])?;
        let [id] = shape(binding, ["identifier"])?;
        let name = self.text(id);
        if !msg.starts_with("Type 'Lazy<")
            || !msg.contains("has no method 'setValue(")
            || self.text(lazy) != "lazy"
            || !self.unshadowed("lazy")
            || self.resolve(self.root, name, "variable_declaration")? != binding
        {
            return None;
        }
        // Refuse potential writes (including qualified/compound writes and
        // inc/dec), and references that could expose a mutable property.
        if descendants(self.root).any(|n| {
            let target = match n.kind() {
                "assignment" => n.child_by_field_name("left"),
                "unary_expression"
                    if n.child_by_field_name("operator")
                        .is_some_and(|op| matches!(op.kind(), "++" | "--")) =>
                {
                    n.child_by_field_name("argument")
                }
                "callable_reference" => Some(n),
                "navigation_expression" if child(n, "::").is_some() => Some(n),
                _ => None,
            };
            target.is_some_and(|t| {
                descendants(t).any(|id| {
                    id.kind() == "identifier"
                        && self.text(id).trim_matches('`') == name.trim_matches('`')
                })
            })
        }) {
            return None;
        }
        Some((var.byte_range(), "val".into()))
    }

    // Both conversions exist on Collection<E>. The boxed return type alone
    // does not distinguish it from e.g. IntArray, which has no toIntArray().
    fn collection_receiver(&self, receiver: Node<'t>, elem: &str) -> Option<()> {
        let id = match receiver.kind() {
            "identifier" => receiver,
            "call_expression" => {
                let [nav, args] = shape(receiver, ["navigation_expression", "value_arguments"])?;
                let [id, _, method] = shape(nav, ["identifier", ".", "identifier"])?;
                shape(args, ["(", ")"])?;
                if self.text(method) != "sorted" || !self.unshadowed("sorted") {
                    return None;
                }
                id
            }
            _ => return None,
        };
        let ty = self.parameter_type(id)?;
        let param = parent(ty, "parameter")?;
        let fun = parent(
            parent(param, "function_value_parameters")?,
            "function_declaration",
        )?;
        let before = std::iter::successors(param.prev_sibling(), |n| n.prev_sibling())
            .find(|n| !n.is_extra())?;
        if !matches!(before.kind(), "(" | ",") // no vararg
            || fun.parent()? != self.root || child(fun, ".").is_some() || ty.kind() != "user_type"
        {
            return None;
        }
        let [base, args] = shape(ty, ["identifier", "type_arguments"])?;
        let [_, projection, _] = shape(args, ["<", "type_projection", ">"])?;
        let [inner] = shape(projection, ["user_type"])?;
        let base = self.text(base);
        (matches!(
            base,
            "List" | "Collection" | "Set" | "MutableList" | "MutableCollection" | "MutableSet"
        ) && self.unshadowed(base)
            && self.simple_type(inner)? == elem)
            .then_some(())
    }

    fn primitive_array(&self, leaf: Node<'t>, msg: &str) -> Option<Edit> {
        let body = enclosing(leaf, "function_body")?;
        let [_, call] = shape(body, ["=", "call_expression"])?;
        let [nav, args] = shape(call, ["navigation_expression", "value_arguments"])?;
        let [receiver, _, method] = shape(nav, ["_", ".", "identifier"])?;
        shape(args, ["(", ")"])?;
        let expected = self.simple_type(return_type(body)?)?;
        let elem = expected.strip_suffix("Array")?;
        let new = format!("to{expected}");
        if call.start_byte() != leaf.start_byte()
            || !PRIMITIVES.contains(&elem)
            || self.text(method) != "toTypedArray"
            || !msg.ends_with(&format!("expected '{expected}', actual 'Array<{elem}>'."))
            || !self.unshadowed("toTypedArray")
            || !self.unshadowed(&new)
            || !self.unshadowed(elem)
            || !self.unshadowed(expected)
        {
            return None;
        }
        self.collection_receiver(receiver, elem)?;
        Some((method.byte_range(), new))
    }

    fn regex_escape(&self, escape: Node<'t>) -> Option<Edit> {
        if !matches!(
            self.text(escape).as_bytes(),
            [b'\\', b'd' | b'D' | b's' | b'S' | b'w' | b'W']
        ) {
            return None;
        }
        let literal = parent(escape, "string_literal")?;
        let arg = parent(literal, "value_argument")?;
        shape(arg, ["string_literal"])?;
        let args = parent(arg, "value_arguments")?;
        shape(args, ["(", "value_argument", ")"])?;
        let [callee, _] = shape(
            parent(args, "call_expression")?,
            ["identifier", "value_arguments"],
        )?;
        (self.text(callee) == "Regex" && self.unshadowed("Regex"))
            .then(|| insert(escape.start_byte(), "\\"))
    }
}

fn insert(at: usize, s: &str) -> Edit {
    (at..at, s.to_owned())
}

pub(super) fn edit(root: Node<'_>, src: &str, diag: &Value) -> Option<Edit> {
    let pos = usize::try_from(diag["offset"].as_u64()?).ok()?;
    let first = src.get(pos..)?.chars().next()?;
    let leaf = root.descendant_for_byte_range(pos, pos + first.len_utf8())?;
    if leaf.start_byte() != pos
        || leaf.child_count() != 0
        || leaf.is_extra()
        || root
            .named_children(&mut root.walk())
            .any(|n| n.kind() == "import")
    {
        return None;
    }
    let file = Subject { root, src };
    let msg = diag["message"].as_str()?;
    match (diag["code"].as_str()?, leaf.kind()) {
        ("ARGUMENT_TYPE_MISMATCH", "identifier") => file.argument(leaf, msg),
        ("UNSAFE_CALL", ".") => file.safe_call(leaf, msg),
        ("CONST_VAL_WITH_NON_CONST_INITIALIZER", "identifier") => file.const_val(leaf),
        ("SUPERTYPE_NOT_INITIALIZED", "identifier") => file.supertype(leaf),
        ("OPERATOR_MODIFIER_REQUIRED", "identifier") => file.operator(leaf, msg),
        ("DATA_CLASS_NOT_PROPERTY_PARAMETER", "identifier") => file.data_parameter(leaf),
        ("DELEGATE_SPECIAL_FUNCTION_MISSING", "by") => file.lazy_val(leaf, msg),
        ("RETURN_TYPE_MISMATCH", "identifier") => file.primitive_array(leaf, msg),
        ("ILLEGAL_ESCAPE", "string_content") => file.regex_escape(leaf),
        _ => None,
    }
}
