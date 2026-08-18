//! A tool must not advertise an input that its handler never reads.
//!
//! The schemas are executable API documentation, while the handler body is
//! the implementation. Comparing them catches the especially dangerous drift
//! where a request value is echoed back as if it had affected the operation.

use konnect_core::router::registry;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn tools_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(1)
        .expect("crates directory above konnect")
        .join("konnect-core/src/tools")
}

fn tool_sources() -> Vec<(PathBuf, String)> {
    let mut sources = std::fs::read_dir(tools_dir())
        .expect("read tools source directory")
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "rs"))
        .map(|path| {
            let source = std::fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
            (path, source)
        })
        .collect::<Vec<_>>();
    sources.sort_by(|a, b| a.0.cmp(&b.0));
    sources
}

/// Locate the handler named in a tool's registration. Registrations all call a
/// normal `handle_*` function, including the one cross-module dispatch; this
/// keeps the guard tied to the real registration instead of assuming a naming
/// convention or maintaining a tool allowlist.
fn registered_handlers(sources: &[(PathBuf, String)]) -> BTreeMap<String, (PathBuf, String)> {
    let mut handlers = BTreeMap::new();
    for (path, source) in sources {
        let code = masked_rust_code(source);
        let mut cursor = 0;
        while let Some(relative) = code[cursor..].find("tool!(") {
            let start = cursor + relative;
            let next = code[start + 6..]
                .find("tool!(")
                .map_or(code.len(), |offset| start + 6 + offset);
            let registration = &source[start..next];
            let Some(tool) = first_quoted_string(registration) else {
                cursor = start + 6;
                continue;
            };
            let Some(handler_at) = registration.find("handle_") else {
                cursor = next;
                continue;
            };
            let handler = registration[handler_at..]
                .chars()
                .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_')
                .collect::<String>();
            handlers.insert(tool, (path.clone(), handler));
            cursor = next;
        }
    }
    handlers
}

fn first_quoted_string(text: &str) -> Option<String> {
    let start = text.find('"')? + 1;
    let end = text[start..].find('"')? + start;
    Some(text[start..end].to_string())
}

fn function_body<'a>(source: &'a str, function: &str) -> Option<&'a str> {
    let code = masked_rust_code(source);
    let needle = format!("fn {function}(");
    let start = code.find(&needle)?;
    let open = code[start..].find('{')? + start;
    let close = matching_brace(source, open)?;
    Some(&source[open + 1..close])
}

#[derive(Clone)]
struct IndexedBody {
    path: PathBuf,
    source: String,
}

type BodyIndex = BTreeMap<String, Vec<IndexedBody>>;

fn all_function_bodies(sources: &[(PathBuf, String)]) -> BodyIndex {
    let mut functions: BodyIndex = BTreeMap::new();
    for (path, source) in sources {
        let code = masked_rust_code(source);
        let mut cursor = 0usize;
        while let Some(relative) = code[cursor..].find("fn ") {
            let name_start = cursor + relative + 3;
            let name = code[name_start..]
                .chars()
                .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_')
                .collect::<String>();
            let signature_end = name_start + name.len();
            let Some(open) = code[signature_end..].find('{').map(|at| signature_end + at) else {
                cursor = signature_end.max(cursor + 1);
                continue;
            };
            if code[signature_end..open].contains(';') {
                cursor = signature_end.max(cursor + 1);
                continue;
            }
            let Some(close) = matching_brace(source, open) else {
                cursor = open + 1;
                continue;
            };
            if !name.is_empty() {
                functions.entry(name).or_default().push(IndexedBody {
                    path: path.clone(),
                    source: source[open + 1..close].to_string(),
                });
            }
            cursor = close + 1;
        }
    }
    functions
}

fn all_macro_bodies(sources: &[(PathBuf, String)]) -> BodyIndex {
    let mut macros: BodyIndex = BTreeMap::new();
    for (path, source) in sources {
        let code = masked_rust_code(source);
        let mut cursor = 0usize;
        while let Some(relative) = code[cursor..].find("macro_rules!") {
            let name_start = cursor + relative + "macro_rules!".len();
            let name_start = name_start
                + code[name_start..]
                    .find(|ch: char| !ch.is_whitespace())
                    .unwrap_or(0);
            let name = code[name_start..]
                .chars()
                .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_')
                .collect::<String>();
            let Some(open) = code[name_start..].find('{').map(|at| name_start + at) else {
                cursor = name_start + 1;
                continue;
            };
            let Some(close) = matching_brace(source, open) else {
                cursor = open + 1;
                continue;
            };
            macros.entry(name).or_default().push(IndexedBody {
                path: path.clone(),
                source: source[open + 1..close].to_string(),
            });
            cursor = close + 1;
        }
    }
    macros
}

#[derive(Default)]
struct BodyFacts {
    function_calls: std::collections::BTreeSet<String>,
    macro_calls: std::collections::BTreeSet<String>,
    string_literals: std::collections::BTreeSet<String>,
}

/// Extract calls and string literals while ignoring comments, character
/// literals, and string contents when looking for calls. The guard needs only
/// this small subset of Rust syntax; avoiding a textual `contains` sweep over
/// every function also keeps the whole-catalogue test sub-second.
fn body_facts(source: &str) -> BodyFacts {
    let bytes = source.as_bytes();
    let mut facts = BodyFacts::default();
    let mut index = 0usize;

    while index < bytes.len() {
        match bytes[index] {
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                index += 2;
                while index < bytes.len() && bytes[index] != b'\n' {
                    index += 1;
                }
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                index += 2;
                let mut depth = 1usize;
                while index + 1 < bytes.len() && depth > 0 {
                    if bytes[index] == b'/' && bytes[index + 1] == b'*' {
                        depth += 1;
                        index += 2;
                    } else if bytes[index] == b'*' && bytes[index + 1] == b'/' {
                        depth -= 1;
                        index += 2;
                    } else {
                        index += 1;
                    }
                }
            }
            b'"' => {
                let (literal, next) = ordinary_string(source, index);
                facts.string_literals.insert(literal);
                index = next;
            }
            b'b' if bytes.get(index + 1) == Some(&b'"') => {
                let (literal, next) = ordinary_string(source, index + 1);
                facts.string_literals.insert(literal);
                index = next;
            }
            b'r' | b'b' if raw_string_start(bytes, index).is_some() => {
                let (literal, next) = raw_string(source, index);
                facts.string_literals.insert(literal);
                index = next;
            }
            b'\'' => index = character_or_lifetime_end(bytes, index),
            byte if byte == b'_' || byte.is_ascii_alphabetic() => {
                let start = index;
                index += 1;
                while index < bytes.len()
                    && (bytes[index] == b'_' || bytes[index].is_ascii_alphanumeric())
                {
                    index += 1;
                }
                let name = &source[start..index];
                let mut after = index;
                while bytes.get(after).is_some_and(u8::is_ascii_whitespace) {
                    after += 1;
                }
                if bytes.get(after) == Some(&b'(') {
                    facts.function_calls.insert(name.to_string());
                } else if bytes.get(after) == Some(&b'!') {
                    facts.macro_calls.insert(name.to_string());
                }
            }
            _ => index += 1,
        }
    }

    facts
}

fn ordinary_string(source: &str, quote: usize) -> (String, usize) {
    let bytes = source.as_bytes();
    let mut literal = String::new();
    let mut index = quote + 1;
    while index < bytes.len() {
        if bytes[index] == b'\\' {
            if let Some(escaped) = bytes.get(index + 1) {
                literal.push(*escaped as char);
                index += 2;
            } else {
                index += 1;
            }
        } else if bytes[index] == b'"' {
            return (literal, index + 1);
        } else {
            literal.push(bytes[index] as char);
            index += 1;
        }
    }
    (literal, index)
}

fn raw_string_start(bytes: &[u8], start: usize) -> Option<(usize, usize)> {
    let mut quote = start + 1;
    if bytes[start] == b'b' && bytes.get(quote) == Some(&b'r') {
        quote += 1;
    } else if bytes[start] != b'r' {
        return None;
    }
    let mut hashes = 0usize;
    while bytes.get(quote) == Some(&b'#') {
        hashes += 1;
        quote += 1;
    }
    (bytes.get(quote) == Some(&b'"')).then_some((quote, hashes))
}

fn raw_string(source: &str, start: usize) -> (String, usize) {
    let bytes = source.as_bytes();
    let Some((quote, hashes)) = raw_string_start(bytes, start) else {
        return (String::new(), start + 1);
    };
    let content = quote + 1;
    let mut index = content;
    while index < bytes.len() {
        if bytes[index] == b'"'
            && (0..hashes).all(|offset| bytes.get(index + 1 + offset) == Some(&b'#'))
        {
            return (source[content..index].to_string(), index + 1 + hashes);
        }
        index += 1;
    }
    (source[content..].to_string(), bytes.len())
}

fn character_or_lifetime_end(bytes: &[u8], start: usize) -> usize {
    let mut close = start + 1;
    let mut escaped = false;
    while close < bytes.len() && close <= start + 8 {
        if !escaped && bytes[close] == b'\'' {
            return close + 1;
        }
        escaped = !escaped && bytes[close] == b'\\';
        if bytes[close] != b'\\' {
            escaped = false;
        }
        close += 1;
    }
    start + 1
}

/// Replace comments and literals with same-length ASCII whitespace. Byte
/// offsets stay aligned with the original source, so structural searches can
/// use the mask and slice bodies from the real text without treating a code
/// sample in a comment or `"fn fake() {}` in an error message as Rust syntax.
fn masked_rust_code(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut masked = bytes.to_vec();
    let mut index = 0usize;

    while index < bytes.len() {
        let end = match bytes[index] {
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                let mut end = index + 2;
                while end < bytes.len() && bytes[end] != b'\n' {
                    end += 1;
                }
                Some(end)
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                let mut end = index + 2;
                let mut depth = 1usize;
                while end + 1 < bytes.len() && depth > 0 {
                    if bytes[end] == b'/' && bytes[end + 1] == b'*' {
                        depth += 1;
                        end += 2;
                    } else if bytes[end] == b'*' && bytes[end + 1] == b'/' {
                        depth -= 1;
                        end += 2;
                    } else {
                        end += 1;
                    }
                }
                Some(end)
            }
            b'"' => Some(ordinary_string(source, index).1),
            b'b' if bytes.get(index + 1) == Some(&b'"') => {
                Some(ordinary_string(source, index + 1).1)
            }
            b'r' | b'b' if raw_string_start(bytes, index).is_some() => {
                Some(raw_string(source, index).1)
            }
            b'\'' => {
                let end = character_or_lifetime_end(bytes, index);
                (end > index + 1).then_some(end)
            }
            _ => None,
        };

        if let Some(end) = end {
            let masked_end = end.min(masked.len());
            for byte in &mut masked[index..masked_end] {
                *byte = b' ';
            }
            index = end;
        } else {
            index += 1;
        }
    }

    String::from_utf8(masked).expect("mask preserves UTF-8 outside literals")
}

/// Include helper functions and macros reached from the registered handler.
/// This recognizes the source's actual call graph, so a parameter read by
/// `read_xy_pair(args)` or by the shared `ipc!` target check counts without a
/// hand-maintained exemption for either the tool or the parameter.
fn transitive_string_literals(
    root: &str,
    root_path: &Path,
    functions: &BodyIndex,
    macros: &BodyIndex,
) -> std::collections::BTreeSet<String> {
    let mut queue = std::collections::VecDeque::from([(root_path.to_path_buf(), root.to_string())]);
    let mut literals = std::collections::BTreeSet::new();
    let mut seen_functions = std::collections::BTreeSet::new();
    let mut seen_macros = std::collections::BTreeSet::new();

    while let Some((path, body)) = queue.pop_front() {
        let facts = body_facts(&body);
        literals.extend(facts.string_literals);
        for name in facts.function_calls {
            enqueue_called_bodies(&mut queue, &mut seen_functions, functions, &name, &path);
        }
        for name in facts.macro_calls {
            enqueue_called_bodies(&mut queue, &mut seen_macros, macros, &name, &path);
        }
    }

    literals
}

fn enqueue_called_bodies(
    queue: &mut std::collections::VecDeque<(PathBuf, String)>,
    seen: &mut std::collections::BTreeSet<(PathBuf, String, String)>,
    index: &BodyIndex,
    name: &str,
    caller_path: &Path,
) {
    let Some(bodies) = index.get(name) else {
        return;
    };
    let has_same_file_body = bodies.iter().any(|body| body.path == caller_path);
    for body in bodies
        .iter()
        .filter(|body| !has_same_file_body || body.path == caller_path)
    {
        let identity = (body.path.clone(), name.to_string(), body.source.clone());
        if seen.insert(identity) {
            queue.push_back((body.path.clone(), body.source.clone()));
        }
    }
}

/// Match a Rust block while ignoring braces in comments and ordinary/raw
/// strings. This is intentionally a tiny lexer rather than a source regex:
/// handler bodies contain many `format!("{value}")` expressions.
fn matching_brace(source: &str, open: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut depth = 1usize;
    let mut index = open + 1;
    while index < bytes.len() {
        match bytes[index] {
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                index += 2;
                while index < bytes.len() && bytes[index] != b'\n' {
                    index += 1;
                }
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                index += 2;
                let mut comment_depth = 1usize;
                while index + 1 < bytes.len() && comment_depth > 0 {
                    if bytes[index] == b'/' && bytes[index + 1] == b'*' {
                        comment_depth += 1;
                        index += 2;
                    } else if bytes[index] == b'*' && bytes[index + 1] == b'/' {
                        comment_depth -= 1;
                        index += 2;
                    } else {
                        index += 1;
                    }
                }
            }
            b'"' => {
                index += 1;
                while index < bytes.len() {
                    if bytes[index] == b'\\' {
                        index += 2;
                    } else if bytes[index] == b'"' {
                        index += 1;
                        break;
                    } else {
                        index += 1;
                    }
                }
            }
            b'\'' => {
                // A character literal can contain a double quote or brace;
                // without this branch the tiny lexer mistakes `\'"\'` for the
                // start of a string. Lifetimes such as `\'a` have no nearby
                // closing quote and are deliberately left alone.
                let mut close = index + 1;
                let mut escaped = false;
                while close < bytes.len() && close <= index + 8 {
                    if !escaped && bytes[close] == b'\'' {
                        break;
                    }
                    escaped = !escaped && bytes[close] == b'\\';
                    if bytes[close] != b'\\' {
                        escaped = false;
                    }
                    close += 1;
                }
                if close < bytes.len() && close <= index + 8 && bytes[close] == b'\'' {
                    index = close + 1;
                } else {
                    index += 1;
                }
            }
            b'r' | b'b' => {
                let mut quote = index + 1;
                if bytes[index] == b'b' && bytes.get(quote) == Some(&b'r') {
                    quote += 1;
                }
                let mut hashes = 0usize;
                while bytes.get(quote) == Some(&b'#') {
                    hashes += 1;
                    quote += 1;
                }
                if bytes.get(quote) != Some(&b'"') {
                    index += 1;
                    continue;
                }
                index = quote + 1;
                while index < bytes.len() {
                    if bytes[index] == b'"'
                        && (0..hashes).all(|offset| bytes.get(index + 1 + offset) == Some(&b'#'))
                    {
                        index += 1 + hashes;
                        break;
                    }
                    index += 1;
                }
            }
            b'{' => {
                depth += 1;
                index += 1;
            }
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(index);
                }
                index += 1;
            }
            _ => index += 1,
        }
    }
    None
}

#[test]
fn lexer_ignores_comments_and_calls_written_inside_literals() {
    let facts = body_facts(
        r####"
        args["direct_field"];
        // args["commented_field"]; fake_comment_call();
        /* nested /* args["nested_comment"] */ fake_block_call(); */
        let _message = "fake_string_call(\"string_field\")";
        let _raw = r#"raw_field and fake_raw_call()"#;
        real_helper();
        real_macro!();
        let _brace = '}';
        "####,
    );

    assert!(facts.string_literals.contains("direct_field"));
    assert!(facts
        .string_literals
        .contains("fake_string_call(\"string_field\")"));
    assert!(facts
        .string_literals
        .contains("raw_field and fake_raw_call()"));
    assert!(!facts.string_literals.contains("commented_field"));
    assert!(!facts.string_literals.contains("nested_comment"));
    assert!(facts.function_calls.contains("real_helper"));
    assert!(!facts.function_calls.contains("fake_string_call"));
    assert!(!facts.function_calls.contains("fake_comment_call"));
    assert!(facts.macro_calls.contains("real_macro"));
}

#[test]
fn function_index_ignores_function_examples_in_comments_and_strings() {
    let source = r####"
        // fn comment_only() { args["ghost"] }
        const EXAMPLE: &str = r#"fn string_only() { args["ghost"] }"#;
        fn real_handler() { helper(args); }
    "####;
    let sources = vec![(PathBuf::from("fixture.rs"), source.to_string())];
    let functions = all_function_bodies(&sources);

    assert!(functions.contains_key("real_handler"));
    assert!(!functions.contains_key("comment_only"));
    assert!(!functions.contains_key("string_only"));
    assert!(function_body(source, "real_handler").is_some());
}

#[test]
fn transitive_analysis_follows_helpers_and_macros_without_an_allowlist() {
    let fixture = PathBuf::from("fixture.rs");
    let functions = BTreeMap::from([(
        "read_option".to_string(),
        vec![IndexedBody {
            path: fixture.clone(),
            source: r#"args["helper_field"];"#.to_string(),
        }],
    )]);
    let macros = BTreeMap::from([(
        "read_shared".to_string(),
        vec![IndexedBody {
            path: fixture.clone(),
            source: r#"args["macro_field"];"#.to_string(),
        }],
    )]);
    let literals = transitive_string_literals(
        r#"args["direct_field"]; read_option(args); read_shared!(args);"#,
        &fixture,
        &functions,
        &macros,
    );

    assert_eq!(
        literals,
        std::collections::BTreeSet::from([
            "direct_field".to_string(),
            "helper_field".to_string(),
            "macro_field".to_string(),
        ])
    );
}

#[test]
fn transitive_analysis_prefers_the_same_file_for_duplicate_helper_names() {
    let routing = PathBuf::from("pcb_routing.rs");
    let components = PathBuf::from("pcb_components.rs");
    let macros = BTreeMap::from([(
        "ipc".to_string(),
        vec![
            IndexedBody {
                path: components,
                source: r#"args["unrelated_field"];"#.to_string(),
            },
            IndexedBody {
                path: routing.clone(),
                source: r#"args["routing_field"];"#.to_string(),
            },
        ],
    )]);
    let literals = transitive_string_literals(
        "ipc!(ctx, args, operation);",
        &routing,
        &BTreeMap::new(),
        &macros,
    );

    assert!(literals.contains("routing_field"));
    assert!(!literals.contains("unrelated_field"));
}

#[test]
fn every_declared_parameter_is_read_by_its_registered_handler() {
    let sources = tool_sources();
    let handlers = registered_handlers(&sources);
    let functions = all_function_bodies(&sources);
    let macros = all_macro_bodies(&sources);
    let mut unread = Vec::new();

    for tool in registry::ALL_TOOLSETS
        .iter()
        .flat_map(|toolset| registry::tools_for(toolset.name).unwrap_or_default())
    {
        let Some((registered_in, handler)) = handlers.get(tool.name) else {
            unread.push(format!("{}: registration handler was not found", tool.name));
            continue;
        };
        let located = sources
            .iter()
            .filter(|(path, _)| path == registered_in)
            .find_map(|(path, source)| {
                function_body(source, handler).map(|body| (path.as_path(), body))
            })
            .or_else(|| {
                sources.iter().find_map(|(path, source)| {
                    function_body(source, handler).map(|body| (path.as_path(), body))
                })
            });
        let Some((body_path, body)) = located else {
            unread.push(format!("{}: function {handler} was not found", tool.name));
            continue;
        };
        let literals = transitive_string_literals(body, body_path, &functions, &macros);
        let properties = tool.input_schema["properties"]
            .as_object()
            .into_iter()
            .flat_map(|object| object.keys());
        for property in properties {
            if !literals.contains(property) {
                unread.push(format!(
                    "{}.{property}: {handler} never reads this declared parameter",
                    tool.name
                ));
            }
        }
    }

    assert!(
        unread.is_empty(),
        "tool schemas advertise ignored parameters:\n  {}\n\n\
         Read and apply the parameter, or remove it from the public schema. Do \
         not report a request value as though it affected the result.",
        unread.join("\n  ")
    );
}
