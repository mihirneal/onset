//! `{column}` templates, e.g. `text = "{word}"` or `stimulus = "feedback_{outcome}"`.
//!
//! `{{` and `}}` produce literal braces.

use std::collections::BTreeMap;

pub type Vars = BTreeMap<String, String>;

/// Variables only known while the run is happening.
pub const RUNTIME_VARS: &[&str] = &["outcome"];

/// Names referenced by a template, in order of appearance.
pub fn references(template: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
            }
            '{' => {
                let name: String = chars.by_ref().take_while(|&c| c != '}').collect();
                out.push(name.trim().to_string());
            }
            _ => {}
        }
    }
    out
}

/// Fill every `{name}` from `vars`. Unknown names are an error that lists
/// what *is* available, since that's almost always a typo or missing column.
pub fn fill(template: &str, vars: &Vars) -> Result<String, String> {
    let mut out = String::with_capacity(template.len());
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                out.push('{');
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                out.push('}');
            }
            '{' => {
                let mut name = String::new();
                let mut closed = false;
                for c in chars.by_ref() {
                    if c == '}' {
                        closed = true;
                        break;
                    }
                    name.push(c);
                }
                if !closed {
                    return Err(format!("unclosed `{{` in {template:?}"));
                }
                let name = name.trim();
                match vars.get(name) {
                    Some(v) => out.push_str(v),
                    None => {
                        let known: Vec<&str> = vars.keys().map(String::as_str).collect();
                        return Err(format!(
                            "{template:?} uses {{{name}}}, but there's no such column (available: {})",
                            known.join(", ")
                        ));
                    }
                }
            }
            c => out.push(c),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> Vars {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn fills_and_escapes() {
        let v = vars(&[("word", "RED"), ("n", "3")]);
        assert_eq!(fill("{word} x{n}", &v).unwrap(), "RED x3");
        assert_eq!(fill("{{literal}}", &v).unwrap(), "{literal}");
        assert_eq!(fill("no templates", &v).unwrap(), "no templates");
    }

    #[test]
    fn unknown_names_list_available_columns() {
        let err = fill("{wrod}", &vars(&[("word", "RED")])).unwrap_err();
        assert!(err.contains("{wrod}") && err.contains("word"), "{err}");
    }

    #[test]
    fn finds_references() {
        assert_eq!(references("fb_{outcome} {{x}} {a}"), vec!["outcome", "a"]);
    }
}
