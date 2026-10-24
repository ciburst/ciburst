//! The label selector language of the API overview ("Label Selector"): `k==v` or `k=v`,
//! `k!=v`, `k`, `!k`, `k in (v1,v2)`, `k notin (v1,v2)`, joined by commas as AND. A key that
//! is missing does not equal any value, as in Kubernetes, whose selectors these follow.

use crate::vm::{Labels, valid_label_key, valid_label_value};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Requirement {
    Equals(String, String),
    NotEquals(String, String),
    Exists(String),
    NotExists(String),
    In(String, Vec<String>),
    NotIn(String, Vec<String>),
}

impl Requirement {
    pub fn matches(&self, labels: &Labels) -> bool {
        let value = |key: &str| labels.get(key).map(String::as_str);
        match self {
            Requirement::Equals(key, wanted) => value(key) == Some(wanted.as_str()),
            Requirement::NotEquals(key, wanted) => value(key) != Some(wanted.as_str()),
            Requirement::Exists(key) => labels.contains_key(key),
            Requirement::NotExists(key) => !labels.contains_key(key),
            Requirement::In(key, set) => value(key).is_some_and(|v| set.iter().any(|s| s == v)),
            Requirement::NotIn(key, set) => value(key).is_none_or(|v| set.iter().all(|s| s != v)),
        }
    }
}

/// Every requirement of `selector`, or why it does not parse.
pub fn parse(selector: &str) -> Result<Vec<Requirement>, String> {
    let mut terms = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    for (index, c) in selector.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => {
                terms.push(&selector[start..index]);
                start = index + 1;
            }
            _ => {}
        }
        if depth < 0 {
            return Err("unbalanced parentheses".into());
        }
    }
    if depth != 0 {
        return Err("unbalanced parentheses".into());
    }
    terms.push(&selector[start..]);
    terms.into_iter().map(term).collect()
}

/// Whether `labels` satisfy every requirement.
pub fn matches(requirements: &[Requirement], labels: &Labels) -> bool {
    requirements.iter().all(|r| r.matches(labels))
}

fn key(text: &str) -> Result<String, String> {
    let text = text.trim();
    if valid_label_key(text) {
        Ok(text.to_string())
    } else {
        Err(format!("invalid label key {text:?}"))
    }
}

fn value(text: &str) -> Result<String, String> {
    let text = text.trim();
    if valid_label_value(text) {
        Ok(text.to_string())
    } else {
        Err(format!("invalid label value {text:?}"))
    }
}

fn set(text: &str) -> Result<Vec<String>, String> {
    let inner = text
        .trim()
        .strip_prefix('(')
        .and_then(|t| t.strip_suffix(')'))
        .ok_or_else(|| format!("expected a parenthesized list in {text:?}"))?;
    inner.split(',').map(value).collect()
}

fn term(text: &str) -> Result<Requirement, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("empty requirement".into());
    }
    if let Some(rest) = text.strip_prefix('!') {
        return Ok(Requirement::NotExists(key(rest)?));
    }
    if let Some((k, rest)) = text.split_once(" notin ") {
        return Ok(Requirement::NotIn(key(k)?, set(rest)?));
    }
    if let Some((k, rest)) = text.split_once(" in ") {
        return Ok(Requirement::In(key(k)?, set(rest)?));
    }
    if let Some((k, v)) = text.split_once("!=") {
        return Ok(Requirement::NotEquals(key(k)?, value(v)?));
    }
    if let Some((k, v)) = text.split_once("==") {
        return Ok(Requirement::Equals(key(k)?, value(v)?));
    }
    if let Some((k, v)) = text.split_once('=') {
        return Ok(Requirement::Equals(key(k)?, value(v)?));
    }
    Ok(Requirement::Exists(key(text)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(items: &[(&str, &str)]) -> Labels {
        items
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn check(selector: &str, items: &[(&str, &str)]) -> bool {
        matches(&parse(selector).expect(selector), &labels(items))
    }

    #[test]
    fn evaluates_every_operator() {
        let vm = [("ciburst", "true"), ("ciburst-name", "demo"), ("env", "")];
        assert!(check("ciburst==true", &vm));
        assert!(check("ciburst=true", &vm));
        assert!(!check("ciburst==false", &vm));
        assert!(check("ciburst-name!=other", &vm));
        assert!(
            check("missing!=x", &vm),
            "a missing key differs from any value"
        );
        assert!(!check("ciburst-name!=demo", &vm));
        assert!(check("ciburst", &vm));
        assert!(check("env", &vm));
        assert!(!check("missing", &vm));
        assert!(check("!missing", &vm));
        assert!(!check("!ciburst", &vm));
        assert!(check("ciburst-name in (a,demo)", &vm));
        assert!(!check("ciburst-name in (a,b)", &vm));
        assert!(check("ciburst-name notin (a,b)", &vm));
        assert!(check("missing notin (a)", &vm));
        assert!(!check("missing in (a)", &vm));
        assert!(check("ciburst==true,ciburst-name==demo", &vm));
        assert!(!check("ciburst==true,ciburst-name==other", &vm));
        assert!(check(" ciburst == true , !missing ", &vm));
        assert!(check("ciburst-name in (a, demo),ciburst", &vm));
    }

    #[test]
    fn refuses_malformed_selectors() {
        for bad in [
            "", ",", "a==b,", "a==-b", "a b==c", "a in (b", "a in b)", "a in b", "!", "a==b==c",
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
    }
}
