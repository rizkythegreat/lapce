use std::{collections::HashMap, env, path::PathBuf};

use anyhow::{Result, anyhow};
use lapce_rpc::plugin::VoltID;
use lsp_types::{DocumentFilter, Url};
use serde_json::{Value, json};

use super::{PluginCatalogRpcHandler, lsp::LspClient};

const LANGUAGES: &[&str] = &[
    "html",
    "javascript",
    "javascriptreact",
    "typescript",
    "typescriptreact",
    "vue",
    "svelte",
    "astro",
    "css",
    "scss",
    "less",
    "php",
    "handlebars",
];

pub(super) fn supports_language(language: &str) -> bool {
    LANGUAGES.contains(&language)
}

pub(super) fn enabled(config: Option<&HashMap<String, Value>>) -> bool {
    config
        .and_then(|c| c.get("enabled"))
        .and_then(Value::as_bool)
        .unwrap_or(true)
}

pub(super) fn is_builtin(id: &VoltID) -> bool {
    id.author == "lapce" && id.name == "tailwindcss"
}

// Use the JavaScript entry point on Windows: npm's .cmd shims cannot be
// spawned directly by Command. The same path also works with pnpm symlinks.
fn server_command(
    workspace: &std::path::Path,
    config: Option<&HashMap<String, Value>>,
) -> Result<(Url, Vec<String>)> {
    let configured = config
        .and_then(|c| c.get("server-path"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let script = "@tailwindcss/language-server/bin/tailwindcss-language-server";
    let path = if let Some(configured) = configured {
        let path = PathBuf::from(configured);
        Some(if path.is_absolute() {
            path
        } else {
            workspace.join(path)
        })
    } else {
        workspace
            .ancestors()
            .map(|dir| dir.join("node_modules").join(script))
            .find(|path| path.is_file())
            .or_else(|| {
                env::var_os("PATH").and_then(|paths| {
                    env::split_paths(&paths).find_map(|dir| {
                        let executable = dir.join("tailwindcss-language-server");
                        if cfg!(windows) {
                            let script = dir.join("node_modules").join(script);
                            script.is_file().then_some(script)
                        } else {
                            executable.is_file().then_some(executable)
                        }
                    })
                })
            })
    }
    .ok_or_else(|| anyhow!("language server not found in node_modules or PATH"))?;

    if !path.is_file() {
        return Err(anyhow!(
            "language server does not exist: {}",
            path.display()
        ));
    }
    // Running npm's entry point through node also avoids changing permissions
    // on installed packages or relying on executable bits on project files.
    Ok((
        Url::parse("urn:node")?,
        vec![path.to_string_lossy().into_owned(), "--stdio".into()],
    ))
}

pub(super) fn start(
    workspace: PathBuf,
    config: Option<&HashMap<String, Value>>,
    plugin_rpc: PluginCatalogRpcHandler,
) -> Result<()> {
    let (server_uri, args) = server_command(&workspace, config)?;
    let settings = config
        .and_then(|c| c.get("settings"))
        .cloned()
        .unwrap_or_else(|| json!({}));
    LspClient::start(
        plugin_rpc,
        LANGUAGES
            .iter()
            .map(|language| DocumentFilter {
                language: Some((*language).into()),
                scheme: Some("file".into()),
                pattern: None,
            })
            .collect(),
        Some(workspace),
        VoltID {
            author: "lapce".into(),
            name: "tailwindcss".into(),
        },
        "Tailwind CSS".into(),
        None,
        None,
        None,
        server_uri,
        args,
        Some(json!({ "settings": settings })),
    )?;
    Ok(())
}

pub(super) fn configuration(settings: &Value, section: Option<&str>) -> Value {
    match section {
        None | Some("") => settings.clone(),
        Some(section) => section
            .split('.')
            .try_fold(settings, |value, key| value.get(key))
            .cloned()
            .unwrap_or(Value::Null),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configuration_sections_and_missing_values() {
        let settings = json!({"tailwindCSS": {"suggestions": true}});
        assert_eq!(configuration(&settings, None), settings);
        assert_eq!(
            configuration(&settings, Some("tailwindCSS.suggestions")),
            json!(true)
        );
        assert_eq!(configuration(&settings, Some("editor")), Value::Null);
    }

    #[test]
    fn can_disable_builtin_server() {
        assert!(enabled(None));
        let config = HashMap::from([("enabled".into(), json!(false))]);
        assert!(!enabled(Some(&config)));
        assert!(supports_language("typescriptreact"));
        assert!(!supports_language("rust"));
    }

    #[test]
    fn resolves_project_and_configured_servers() {
        let workspace = env::temp_dir()
            .join(format!("lapce-tailwind-{}", std::process::id()));
        std::fs::create_dir_all(&workspace).unwrap();
        let local = workspace.join(
            "node_modules/@tailwindcss/language-server/bin/tailwindcss-language-server",
        );
        std::fs::create_dir_all(local.parent().unwrap()).unwrap();
        std::fs::write(&local, "").unwrap();
        let nested = workspace.join("packages/web");
        std::fs::create_dir_all(&nested).unwrap();
        let (_, args) = server_command(&nested, None).unwrap();
        assert_eq!(args[0], local.to_string_lossy().as_ref());

        let script = workspace.join("server.js");
        std::fs::write(&script, "").unwrap();
        let mut config =
            HashMap::from([("server-path".into(), json!("server.js"))]);
        let (uri, args) = server_command(&workspace, Some(&config)).unwrap();
        assert_eq!(uri.as_str(), "urn:node");
        assert_eq!(
            args,
            vec![script.to_string_lossy().into_owned(), "--stdio".into()]
        );
        config.insert("server-path".into(), json!("missing.js"));
        assert!(server_command(&workspace, Some(&config)).is_err());
        std::fs::remove_dir_all(workspace).unwrap();
    }
}
