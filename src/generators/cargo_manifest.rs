//! Minimal, formatting-preserving edits of `Cargo.toml` for
//! `generate library` / `generate sub-app` (NestJS monorepo equivalents).
//!
//! Edits are line-based so comments and layout survive; every result is
//! re-parsed to make sure the manifest is still valid TOML.

use crate::error::{CliError, CliResult};

/// Dependencies a new library crate copies from the application, so that
/// Armature's macros resolve (`#[module]`/`#[injectable]` expand to
/// `armature_core::…` and `tracing::…` paths).
const INHERITED: &[&str] = &[
    "armature",
    "armature-core",
    "tracing",
    "async-trait",
    "serde",
    "serde_json",
    "tokio",
];

/// `name = <spec>` lines for the [`INHERITED`] dependencies of `manifest`.
pub fn inherited_dependencies(manifest: &str) -> CliResult<String> {
    let value: toml::Value = toml::from_str(manifest)?;
    let Some(deps) = value.get("dependencies").and_then(|d| d.as_table()) else {
        return Ok(String::new());
    };
    let mut out = String::new();
    for name in INHERITED {
        if let Some(spec) = deps.get(*name) {
            out.push_str(&format!("{name} = {}\n", inline(spec)));
        }
    }
    Ok(out)
}

/// TOML inline representation of a dependency spec, keys in the usual
/// order (`version`, `package`, `path`, … `features` last).
fn inline(value: &toml::Value) -> String {
    const ORDER: &[&str] = &["version", "package", "path", "git", "branch", "tag", "rev"];
    let rank = |k: &str| match ORDER.iter().position(|o| *o == k) {
        Some(i) => i,
        None if k == "features" => usize::MAX,
        None => ORDER.len(),
    };
    match value {
        toml::Value::Table(table) => {
            let mut entries: Vec<(&String, &toml::Value)> = table.iter().collect();
            entries.sort_by_key(|(k, _)| rank(k));
            let items: Vec<String> = entries
                .iter()
                .map(|(k, v)| format!("{k} = {}", inline(v)))
                .collect();
            format!("{{ {} }}", items.join(", "))
        }
        toml::Value::Array(items) => {
            let items: Vec<String> = items.iter().map(inline).collect();
            format!("[{}]", items.join(", "))
        }
        other => other.to_string(),
    }
}

/// Add `member` to `[workspace] members`, creating the section if needed.
pub fn add_workspace_member(manifest: &str, member: &str) -> CliResult<String> {
    let value: toml::Value = toml::from_str(manifest)?;
    let members = value
        .get("workspace")
        .and_then(|w| w.get("members"))
        .and_then(|m| m.as_array());
    if members.is_some_and(|m| m.iter().any(|x| x.as_str() == Some(member))) {
        return Ok(manifest.to_string());
    }

    let lines: Vec<&str> = manifest.lines().collect();
    let section = lines.iter().position(|l| l.trim() == "[workspace]");
    let out = match section {
        None => format!(
            "{}\n\n[workspace]\nmembers = [\"{member}\"]\n",
            manifest.trim_end()
        ),
        Some(start) => {
            let end = lines[start + 1..]
                .iter()
                .position(|l| l.trim_start().starts_with('['))
                .map_or(lines.len(), |i| start + 1 + i);
            let members_line = (start + 1..end).find(|&i| {
                let t = lines[i].trim_start();
                t.starts_with("members") && t[7..].trim_start().starts_with('=')
            });
            let mut out: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
            match members_line {
                None => out.insert(start + 1, format!("members = [\"{member}\"]")),
                Some(i) => {
                    // Find the closing bracket (the list may span lines).
                    let close = (i..end).find(|&j| out[j].contains(']')).ok_or_else(|| {
                        CliError::Config("unterminated `members` list in Cargo.toml".into())
                    })?;
                    let line = out[close].clone();
                    let pos = line.rfind(']').unwrap_or(line.len());
                    let before = line[..pos].trim_end();
                    let needs_comma = !(before.ends_with('[') || before.ends_with(','));
                    let insertion = if close == i {
                        format!("{}\"{member}\"", if needs_comma { ", " } else { "" })
                    } else {
                        format!("    \"{member}\",\n")
                    };
                    let new_line = if close == i {
                        format!("{before}{insertion}{}", &line[pos..])
                    } else {
                        // Multi-line list: add an entry before the bracket line.
                        let prev = close - 1;
                        if !out[prev].trim_end().ends_with(',')
                            && !out[prev].trim_end().ends_with('[')
                        {
                            out[prev] = format!("{},", out[prev].trim_end());
                        }
                        format!("{insertion}{line}")
                    };
                    out[close] = new_line;
                }
            }
            let mut text = out.join("\n");
            if manifest.ends_with('\n') {
                text.push('\n');
            }
            text
        }
    };
    toml::from_str::<toml::Value>(&out)
        .map_err(|e| CliError::Config(format!("could not update Cargo.toml: {e}")))?;
    Ok(out)
}

/// Add `name = { path = "<path>" }` to `[dependencies]`.
pub fn add_path_dependency(manifest: &str, name: &str, path: &str) -> CliResult<String> {
    let value: toml::Value = toml::from_str(manifest)?;
    if value
        .get("dependencies")
        .and_then(|d| d.get(name))
        .is_some()
    {
        return Ok(manifest.to_string());
    }
    let line = format!("{name} = {{ path = \"{path}\" }}");
    let lines: Vec<&str> = manifest.lines().collect();
    let out = match lines.iter().position(|l| l.trim() == "[dependencies]") {
        Some(i) => {
            let mut out: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
            out.insert(i + 1, line);
            let mut text = out.join("\n");
            if manifest.ends_with('\n') {
                text.push('\n');
            }
            text
        }
        None => format!("{}\n\n[dependencies]\n{line}\n", manifest.trim_end()),
    };
    toml::from_str::<toml::Value>(&out)
        .map_err(|e| CliError::Config(format!("could not update Cargo.toml: {e}")))?;
    Ok(out)
}

/// Result of a dependency edit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DepEdit {
    Updated(String),
    AlreadyThere,
    /// The manifest layout is not one we can edit safely.
    Manual(String),
}

/// Enable `feature` on the `armature` dependency (single-line spec).
pub fn ensure_armature_feature(manifest: &str, feature: &str) -> CliResult<DepEdit> {
    let value: toml::Value = toml::from_str(manifest)?;
    let Some(spec) = value.get("dependencies").and_then(|d| d.get("armature")) else {
        return Ok(DepEdit::Manual(
            "no `armature` dependency in Cargo.toml".into(),
        ));
    };
    let enabled = spec
        .get("features")
        .and_then(|f| f.as_array())
        .is_some_and(|f| f.iter().any(|x| x.as_str() == Some(feature)));
    if enabled {
        return Ok(DepEdit::AlreadyThere);
    }

    let mut new_spec = match spec {
        toml::Value::String(version) => {
            let mut t = toml::map::Map::new();
            t.insert("version".into(), toml::Value::String(version.clone()));
            t
        }
        toml::Value::Table(t) => t.clone(),
        _ => {
            return Ok(DepEdit::Manual(
                "unexpected `armature` dependency format".into(),
            ));
        }
    };
    let features = new_spec
        .entry("features")
        .or_insert_with(|| toml::Value::Array(Vec::new()));
    if let toml::Value::Array(list) = features {
        list.push(toml::Value::String(feature.to_string()));
    }

    let lines: Vec<&str> = manifest.lines().collect();
    let Some(i) = lines.iter().position(|l| {
        let t = l.trim_start();
        t.starts_with("armature ") || t.starts_with("armature=")
    }) else {
        return Ok(DepEdit::Manual(
            "`armature` is not declared on one line".into(),
        ));
    };
    if !lines[i].trim_end().ends_with('}') && !lines[i].trim_end().ends_with('"') {
        return Ok(DepEdit::Manual(
            "`armature` is not declared on one line".into(),
        ));
    }
    let mut out: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
    out[i] = format!("armature = {}", inline(&toml::Value::Table(new_spec)));
    let mut text = out.join("\n");
    if manifest.ends_with('\n') {
        text.push('\n');
    }
    let reparsed: toml::Value = toml::from_str(&text)
        .map_err(|e| CliError::Config(format!("could not update Cargo.toml: {e}")))?;
    if reparsed
        .get("dependencies")
        .and_then(|d| d.get("armature"))
        .is_none()
    {
        return Ok(DepEdit::Manual(
            "could not update the `armature` dependency".into(),
        ));
    }
    Ok(DepEdit::Updated(text))
}

/// Add `name = <spec>` to `[dependencies]` unless already declared.
pub fn ensure_dependency(manifest: &str, name: &str, spec: &str) -> CliResult<DepEdit> {
    let value: toml::Value = toml::from_str(manifest)?;
    if value
        .get("dependencies")
        .and_then(|d| d.get(name))
        .is_some()
    {
        return Ok(DepEdit::AlreadyThere);
    }
    let line = format!("{name} = {spec}");
    let lines: Vec<&str> = manifest.lines().collect();
    let out = match lines.iter().position(|l| l.trim() == "[dependencies]") {
        Some(i) => {
            // After the last line of the section.
            let end = lines[i + 1..]
                .iter()
                .position(|l| l.trim_start().starts_with('['))
                .map_or(lines.len(), |j| i + 1 + j);
            let mut insert_at = end;
            while insert_at > i + 1 && lines[insert_at - 1].trim().is_empty() {
                insert_at -= 1;
            }
            let mut out: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
            out.insert(insert_at, line);
            let mut text = out.join("\n");
            if manifest.ends_with('\n') {
                text.push('\n');
            }
            text
        }
        None => format!("{}\n\n[dependencies]\n{line}\n", manifest.trim_end()),
    };
    toml::from_str::<toml::Value>(&out)
        .map_err(|e| CliError::Config(format!("could not update Cargo.toml: {e}")))?;
    Ok(DepEdit::Updated(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    const APP: &str = "[package]\nname = \"shop\"\nversion = \"0.1.0\"\n\n[dependencies]\narmature = { version = \"0.8\", package = \"armature-framework\" }\n# comment kept\narmature-core = \"0.11\"\ntokio = { version = \"1.0\", features = [\"full\"] }\nheck = \"0.5\"\n";

    #[test]
    fn inherits_armature_dependencies_only() {
        let deps = inherited_dependencies(APP).unwrap();
        assert!(
            deps.contains("armature = { version = \"0.8\", package = \"armature-framework\" }\n"),
            "{deps}"
        );
        assert!(deps.contains("armature-core = \"0.11\"\n"));
        assert!(
            deps.contains("tokio = { version = \"1.0\", features = [\"full\"] }\n"),
            "{deps}"
        );
        assert!(!deps.contains("heck"));
    }

    #[test]
    fn workspace_members() {
        let out = add_workspace_member(APP, "libs/shared").unwrap();
        assert!(
            out.ends_with("[workspace]\nmembers = [\"libs/shared\"]\n"),
            "{out}"
        );
        assert!(out.contains("# comment kept"));
        let out = add_workspace_member(&out, "apps/admin").unwrap();
        assert!(
            out.contains("members = [\"libs/shared\", \"apps/admin\"]"),
            "{out}"
        );
        assert_eq!(add_workspace_member(&out, "apps/admin").unwrap(), out);

        let multi = "[workspace]\nmembers = [\n    \"a\"\n]\n";
        let out = add_workspace_member(multi, "b").unwrap();
        assert_eq!(out, "[workspace]\nmembers = [\n    \"a\",\n    \"b\",\n]\n");

        let empty = "[package]\nname = \"x\"\n\n[workspace]\n\n[dependencies]\n";
        let out = add_workspace_member(empty, "b").unwrap();
        assert!(out.contains("[workspace]\nmembers = [\"b\"]\n"), "{out}");
    }

    #[test]
    fn armature_features() {
        let DepEdit::Updated(out) = ensure_armature_feature(APP, "auth").unwrap() else {
            panic!()
        };
        assert!(out.contains("armature = { version = \"0.8\", package = \"armature-framework\", features = [\"auth\"] }"), "{out}");
        let DepEdit::Updated(out) = ensure_armature_feature(&out, "cron").unwrap() else {
            panic!()
        };
        assert!(out.contains("features = [\"auth\", \"cron\"]"));
        assert_eq!(
            ensure_armature_feature(&out, "auth").unwrap(),
            DepEdit::AlreadyThere
        );

        let plain = "[dependencies]\narmature = \"0.8\"\n";
        let DepEdit::Updated(out) = ensure_armature_feature(plain, "auth").unwrap() else {
            panic!()
        };
        assert_eq!(
            out,
            "[dependencies]\narmature = { version = \"0.8\", features = [\"auth\"] }\n"
        );

        let table = "[dependencies.armature]\nversion = \"0.8\"\n";
        assert!(matches!(
            ensure_armature_feature(table, "auth").unwrap(),
            DepEdit::Manual(_)
        ));
    }

    #[test]
    fn extra_dependencies() {
        let DepEdit::Updated(out) = ensure_dependency(APP, "chrono", "\"0.4\"").unwrap() else {
            panic!()
        };
        assert!(out.ends_with("heck = \"0.5\"\nchrono = \"0.4\"\n"), "{out}");
        assert_eq!(
            ensure_dependency(&out, "chrono", "\"0.4\"").unwrap(),
            DepEdit::AlreadyThere
        );
    }

    #[test]
    fn path_dependencies() {
        let out = add_path_dependency(APP, "shared", "libs/shared").unwrap();
        assert!(out.contains("[dependencies]\nshared = { path = \"libs/shared\" }\narmature ="));
        assert_eq!(
            add_path_dependency(&out, "shared", "libs/shared").unwrap(),
            out
        );
    }
}
