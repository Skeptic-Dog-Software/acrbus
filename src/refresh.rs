//! Keeping what setup wrote current with the binary that runs it.
//!
//! Setup writes files once: the Claude Code plugin, the reviewers'
//! confinement and system prompt, bridges' tables and ACR's bindings. A
//! newer binary brings newer versions of each, so what it owns outright it
//! rewrites when the daemon starts, and what holds a person's choices too,
//! the config and the bindings, `setup refresh` brings up to date, backing
//! each up first and saying what it changed.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::bridge::Kind;
use crate::config::{APP_NAME, Paths};
use crate::setup;

const VERSION: &str = env!("CARGO_PKG_VERSION");

// ------------------------------------------------------------ owned files

/// The files setup writes that hold nothing of a person's, which this
/// binary rewrites when they aren't its own version: the plugin (when it's
/// installed), the `/pipe` skill (when setup wrote it), the reviewers'
/// confinement and the system prompt (when setup acr made them). Each line
/// says what was rewritten, or with `apply` false, what's due. `exe` is the
/// installed binary the plugin runs; without one, the plugin isn't touched.
pub fn owned(paths: &Paths, exe: Option<&Path>, apply: bool) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let plugin = setup::plugin_dir()?;
    let manifest = plugin.join(".claude-plugin/plugin.json");
    if let Ok(text) = fs::read_to_string(&manifest) {
        let was = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| v["version"].as_str().map(str::to_string))
            .unwrap_or_else(|| "an unknown version".to_string());
        let runs = fs::read_to_string(plugin.join(".mcp.json"))
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            .and_then(|v| {
                v["mcpServers"][crate::config::app().name]["command"]
                    .as_str()
                    .map(PathBuf::from)
            });
        let moved = exe.is_some_and(|e| runs.as_deref() != Some(e));
        if was != VERSION || moved {
            let why = if was != VERSION {
                format!("from {was}")
            } else {
                format!(
                    "running {}",
                    runs.map(|r| r.display().to_string()).unwrap_or_default()
                )
            };
            match (apply, exe) {
                (true, Some(exe)) => {
                    setup::write_plugin(exe)?;
                    out.push(format!(
                        "rewrote the Claude Code plugin ({why}) as {VERSION}, running {}; \
                         sessions load it when they restart",
                        exe.display()
                    ));
                }
                (true, None) => out.push(format!(
                    "the Claude Code plugin is {why}, but this isn't an installed {APP_NAME}, so \
                     it was left as it is"
                )),
                (false, _) => out.push(format!("the Claude Code plugin is {why}")),
            }
        }
    }
    let pipe = setup::pipe_skill_path()?;
    if setup::pipe_skill_state(&pipe) == (setup::PipeSkill::Ours { current: false }) {
        if apply {
            setup::write_pipe_skill(&pipe)?;
            out.push(format!("rewrote the /pipe skill, {}", pipe.display()));
        } else {
            out.push(format!(
                "the /pipe skill, {}, is another version's",
                pipe.display()
            ));
        }
    }
    let acr = paths.config.parent().unwrap_or(Path::new(".")).join("acr");
    let files = [
        (
            acr.join(setup::CONFINE_FILE),
            setup::confine_settings(),
            "reviewers' confinement",
        ),
        (
            setup::system_prompt_path(paths),
            setup::SYSTEM_PROMPT.to_string(),
            "ACR's system prompt",
        ),
    ];
    for (file, text, what) in files {
        match fs::read_to_string(&file) {
            Ok(had) if had != text => {
                if apply {
                    fs::write(&file, &text)
                        .with_context(|| format!("writing {}", file.display()))?;
                    out.push(format!("rewrote the {what}, {}", file.display()));
                } else {
                    out.push(format!(
                        "the {what}, {}, is another version's",
                        file.display()
                    ));
                }
            }
            _ => {}
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------- bridges

/// What moving a bridge's table to its kind's form did.
#[derive(Debug, Default, PartialEq)]
pub struct Moved {
    pub agent: String,
    pub kind: String,
    /// Keys it dropped whose value wasn't the new default, so a person can
    /// put a real choice back: `key`, or `table.key`.
    pub dropped: Vec<String>,
    /// Arguments it carried from the old `args` into `extra_args`.
    pub carried: Vec<String>,
}

/// Flags, each with its value, that an old table's `args` carried and its
/// kind's defaults don't: they're the machine's, so they go to `extra_args`.
const CARRIED_FLAGS: [&str; 6] = [
    "--settings",
    "--append-system-prompt",
    "--system-prompt",
    "--mcp-config",
    "-e",
    "--extension",
];

/// Keys kept even where the kind's defaults set them: they're the machine's.
fn kept_anyway(table: Option<&str>, key: &str) -> bool {
    matches!((table, key), (Some("options"), "workdirs"))
}

/// The config's bridge tables, brought to the form that takes this
/// version's defaults: a table written out in full, with no `kind`, whose
/// command is Claude Code, omp or pi, becomes `kind = "…"` with only what's
/// this machine's. Returns what moved, and for tables already in that form,
/// the keys that keep a newer default from them.
pub fn bridges(doc: &mut toml_edit::DocumentMut) -> Result<(Vec<Moved>, Vec<String>)> {
    let mut moved = Vec::new();
    let mut shadowing = Vec::new();
    let agents: Vec<String> = doc
        .get("bridges")
        .and_then(|b| b.as_table_like())
        .map(|t| t.iter().map(|(k, _)| k.to_string()).collect())
        .unwrap_or_default();
    for agent in agents {
        let item = &doc["bridges"][agent.as_str()];
        if item.as_table_like().is_none() {
            continue;
        }
        let text = table_text(item);
        let old: toml::Value =
            toml::from_str(&text).with_context(|| format!("reading [bridges.{agent}]"))?;
        if let Some(kind) = old.get("kind").and_then(|k| k.as_str()) {
            let kind: Option<Kind> = toml::Value::String(kind.to_string()).try_into().ok();
            if let Some(kind) = kind {
                let shadows = shadowed(&old, kind)?;
                if !shadows.is_empty() {
                    shadowing.push(format!(
                        "[bridges.{agent}] sets {}, which {APP_NAME}'s defaults for {} set \
                         otherwise: a newer default for them won't reach it",
                        shadows.join(", "),
                        kind.name()
                    ));
                }
            }
            continue;
        }
        let Some(kind) = old
            .get("command")
            .and_then(|c| c.as_str())
            .and_then(|c| Kind::of(Path::new(c)))
        else {
            continue;
        };
        let (table, done) = moved_table(&agent, &old, kind)?;
        let position = item.as_table().and_then(|t| t.position());
        let mut table = table;
        if let Some(p) = position {
            table.set_position(Some(p));
        }
        doc["bridges"][agent.as_str()] = toml_edit::Item::Table(table);
        moved.push(done);
    }
    Ok((moved, shadowing))
}

/// A table item as TOML text of its own.
fn table_text(item: &toml_edit::Item) -> String {
    let mut doc = toml_edit::DocumentMut::new();
    if let Some(t) = item.as_table_like() {
        for (k, v) in t.iter() {
            doc.insert(k, v.clone());
        }
    }
    doc.to_string()
}

/// The keys of a table in kind form that set something its kind's defaults
/// set otherwise.
fn shadowed(old: &toml::Value, kind: Kind) -> Result<Vec<String>> {
    let defaults: toml::Value = toml::from_str(kind.defaults())?;
    let mut out = Vec::new();
    let Some(old) = old.as_table() else {
        return Ok(out);
    };
    for (key, value) in old {
        match (value.as_table(), defaults.get(key)) {
            (Some(sub), Some(toml::Value::Table(dsub))) => {
                for (k, v) in sub {
                    if !kept_anyway(Some(key), k) && dsub.get(k).is_some_and(|d| d != v) {
                        out.push(format!("{key}.{k}"));
                    }
                }
            }
            (_, Some(d)) if d != value && key != "model" => out.push(key.clone()),
            _ => {}
        }
    }
    Ok(out)
}

fn edit_value(v: &toml::Value) -> Result<toml_edit::Item> {
    let text = v.to_string();
    let value: toml_edit::Value = text
        .parse()
        .with_context(|| format!("carrying {text} over"))?;
    Ok(toml_edit::Item::Value(value))
}

/// The kind-form table for an old one, and what moving it did.
fn moved_table(agent: &str, old: &toml::Value, kind: Kind) -> Result<(toml_edit::Table, Moved)> {
    let defaults: toml::Value = toml::from_str(kind.defaults())?;
    let mut done = Moved {
        agent: agent.to_string(),
        kind: kind.name().to_string(),
        ..Moved::default()
    };
    let mut table = toml_edit::Table::new();
    table.decor_mut().set_prefix(format!(
        "\n# Its args, options, environment and usage are {APP_NAME}'s defaults for kind\n\
         # \"{}\": this table says only what's this machine's, and anything set\n\
         # here wins. `{APP_NAME} setup refresh` moved it to this form.\n",
        kind.name()
    ));
    table.insert("kind", toml_edit::value(kind.name()));
    let empty = toml::map::Map::new();
    let old = old.as_table().unwrap_or(&empty);
    let dtable = defaults.as_table().unwrap_or(&empty);
    // The old args' own flags go to extra_args.
    let default_args: Vec<&str> = dtable
        .get("args")
        .and_then(|a| a.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    let mut extra: Vec<String> = old
        .get("extra_args")
        .and_then(|a| a.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    if let Some(args) = old.get("args").and_then(|a| a.as_array()) {
        let args: Vec<&str> = args.iter().filter_map(|v| v.as_str()).collect();
        let mut i = 0;
        let mut left = Vec::new();
        while i < args.len() {
            let flag = args[i];
            if CARRIED_FLAGS.contains(&flag) && i + 1 < args.len() && !default_args.contains(&flag)
            {
                let pair = [flag.to_string(), args[i + 1].to_string()];
                if !extra.windows(2).any(|w| w == pair) {
                    extra.extend(pair.iter().cloned());
                }
                done.carried.push(format!("{flag} {}", args[i + 1]));
                i += 2;
                continue;
            }
            left.push(flag);
            i += 1;
        }
        if left != default_args {
            done.dropped.push("args".to_string());
        }
    }
    let order = [
        "accepts",
        "owner",
        "command",
        "working_directory",
        "home",
        "path",
        "model",
    ];
    let mut keys: Vec<&String> = old.keys().collect();
    keys.sort_by_key(|k| order.iter().position(|o| o == k).unwrap_or(order.len()));
    for key in keys {
        let value = &old[key.as_str()];
        match key.as_str() {
            "kind" | "args" | "extra_args" => continue,
            _ => {}
        }
        if let Some(sub) = value.as_table() {
            let dsub = dtable.get(key).and_then(|d| d.as_table());
            let mut kept = toml_edit::Table::new();
            for (k, v) in sub {
                let default = dsub.and_then(|d| d.get(k));
                let keep = key == "secrets"
                    || kept_anyway(Some(key), k)
                    || default.is_none()
                    || (key == "options"
                        && k == "models"
                        && dsub.is_none_or(|d| !d.contains_key("models")));
                if keep {
                    kept.insert(k, edit_value(v)?);
                } else if default != Some(v) {
                    done.dropped.push(format!("{key}.{k}"));
                }
            }
            if !kept.is_empty() {
                table.insert(key, toml_edit::Item::Table(kept));
            }
            continue;
        }
        let default = dtable.get(key);
        let keep = default.is_none() || (key == "model" && default != Some(value));
        if keep {
            table.insert(key, edit_value(value)?);
        } else if default != Some(value) {
            done.dropped.push(key.clone());
        }
    }
    if !extra.is_empty() {
        let mut array = toml_edit::Array::new();
        for a in &extra {
            array.push(a.as_str());
        }
        table.insert("extra_args", toml_edit::value(array));
    }
    // Subtables after the keys, as setup writes them.
    table.sort_values_by(|k1, v1, k2, v2| {
        (v1.is_table(), order_of(k1.get())).cmp(&(v2.is_table(), order_of(k2.get())))
    });
    Ok((table, done))
}

fn order_of(key: &str) -> usize {
    [
        "kind",
        "accepts",
        "owner",
        "command",
        "working_directory",
        "home",
        "path",
        "model",
    ]
    .iter()
    .position(|k| *k == key)
    .unwrap_or(100)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Claude Code bridge as `setup acr` wrote one out in full, before
    /// bridges named their kind: its args carry the confinement.
    const OLD_CLAUDE: &str = r#"[other]
keep = true

[bridges.claude-acr]
owner = "me@example.com"
command = "/home/me/.local/bin/claude"
args = ["-p", "--model", "{MODEL}", "--strict-mcp-config", "--output-format", "text",
  "--settings", "/home/me/.config/acrbus/acr/confine.json",
  "--add-dir", "{ATTACHMENTS_DIR}", "--add-dir", "{RUN_DIR}/tmp"]
model = "claude-sonnet-5-5"
working_directory = "/home/me/.local/state/acrbus/claude-acr/home"
home = "/home/me/.local/state/acrbus/claude-acr/home"
path = "/home/me/.local/bin:/usr/bin:/bin"
timeout_secs = 14400
max_concurrent = 3

[bridges.claude-acr.env]
CLAUDE_CONFIG_DIR = "{SESSION_DIR}"
CLAUDE_CODE_PRINT_BG_WAIT_CEILING_MS = "0"
MY_OWN = "1"

[bridges.claude-acr.secrets]
CLAUDE_CODE_OAUTH_TOKEN = "/home/me/.config/acrbus/claude-token"

[bridges.claude-acr.options]
max_timeout_secs = 14400
tools = ["Read", "Grep", "Glob", "Bash", "Write"]
tools_args = ["--tools", "{TOOLS}", "--allowedTools", "{TOOLS}"]
models = ["claude-haiku-5-5", "claude-sonnet-5-5", "claude-opus-5-5"]
efforts = ["low", "medium", "high"]
effort_args = ["--effort", "{EFFORT}"]
workdirs = ["/home/me/Work/app"]

[bridges.claude-acr.usage]
files = "**/*.jsonl"
when = { type = "assistant" }
dedupe = "message.id"
input = "message.usage.input_tokens"
output = "message.usage.output_tokens"
cache_read = "message.usage.cache_read_input_tokens"
cache_write = "message.usage.cache_creation_input_tokens"
model = "message.model"

[bridges.mine]
command = "/usr/local/bin/my-agent"
args = ["--go"]
"#;

    fn refreshed(text: &str) -> (String, Vec<Moved>, Vec<String>) {
        let mut doc: toml_edit::DocumentMut = text.parse().unwrap();
        let (moved, shadowing) = bridges(&mut doc).unwrap();
        (doc.to_string(), moved, shadowing)
    }

    #[test]
    fn an_old_bridge_moves_to_its_kinds_form_keeping_what_is_the_machines() {
        let (out, moved, _) = refreshed(OLD_CLAUDE);
        assert_eq!(moved.len(), 1, "{out}");
        let m = &moved[0];
        assert_eq!(
            (m.agent.as_str(), m.kind.as_str()),
            ("claude-acr", "claude")
        );
        // The confinement its args carried goes on, in extra_args.
        assert_eq!(
            m.carried,
            vec!["--settings /home/me/.config/acrbus/acr/confine.json"]
        );
        // What differed from the new defaults is said, to put back if meant.
        assert_eq!(m.dropped, vec!["options.efforts", "options.tools"], "{out}");
        let cfg: toml::Value = toml::from_str(&out).unwrap();
        let b = &cfg["bridges"]["claude-acr"];
        assert_eq!(b["kind"].as_str(), Some("claude"));
        assert_eq!(b["owner"].as_str(), Some("me@example.com"));
        assert_eq!(
            b["path"].as_str(),
            Some("/home/me/.local/bin:/usr/bin:/bin")
        );
        assert!(b.get("args").is_none() && b.get("usage").is_none(), "{out}");
        assert!(
            b.get("model").is_none() && b.get("max_concurrent").is_none(),
            "{out}"
        );
        assert_eq!(b["extra_args"].as_array().unwrap().len(), 2, "{out}");
        assert_eq!(
            b["secrets"]["CLAUDE_CODE_OAUTH_TOKEN"].as_str(),
            Some("/home/me/.config/acrbus/claude-token")
        );
        assert_eq!(b["env"]["MY_OWN"].as_str(), Some("1"));
        assert!(b["env"].get("CLAUDE_CONFIG_DIR").is_none(), "{out}");
        assert_eq!(b["options"]["workdirs"].as_array().unwrap().len(), 1);
        assert!(b["options"].get("tools").is_none(), "{out}");
        // Other tables, and a bridge for an agent it doesn't know, stay.
        assert_eq!(cfg["other"]["keep"].as_bool(), Some(true));
        assert_eq!(cfg["bridges"]["mine"]["args"].as_array().unwrap().len(), 1);
        // What it writes reads as the same bridge would, on the defaults.
        let merged = crate::bridge::with_defaults(b.clone()).unwrap();
        assert_eq!(merged["options"]["tools"].as_array().unwrap().len(), 6);
        assert!(
            merged["args"]
                .as_array()
                .unwrap()
                .iter()
                .any(|a| a.as_str() == Some("--strict-mcp-config"))
        );
        // Again, it's already in its kind's form: nothing moves.
        let (again, moved_again, _) = refreshed(&out);
        assert!(moved_again.is_empty());
        assert_eq!(again, out);
    }

    #[test]
    fn a_bridge_in_kind_form_says_what_keeps_a_newer_default_from_it() {
        let text = "[bridges.pi]\nkind = \"pi\"\nowner = \"me\"\ncommand = \"/bin/pi\"\n\
                    max_concurrent = 25\n\n[bridges.pi.options]\nmodels = [\"m\"]\n\
                    workdirs = [\"/w\"]\nefforts = [\"high\"]\n";
        let (out, moved, shadowing) = refreshed(text);
        assert!(moved.is_empty());
        assert_eq!(out, text, "unchanged");
        assert_eq!(shadowing.len(), 1);
        assert!(
            shadowing[0].contains("sets max_concurrent, options.efforts"),
            "{}",
            shadowing[0]
        );
    }
}
