//! URL opening configuration.

use std::error::Error;

use serde::de::{self, Error as SerdeError, MapAccess, Visitor};
use serde::ser::SerializeStruct;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::config::ui_config::Program;

use alacritty_config::SerdeReplace;

const MAX_OPEN_TARGET_LEN: usize = 8192;

/// URL opening configuration.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct OpenConfig {
    /// Fallback launcher when no open action matches.
    pub launcher: OpenLauncher,

    /// URL-specific open actions.
    pub actions: Vec<OpenAction>,
}

impl<'de> Deserialize<'de> for OpenConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct OpenConfigDe {
            #[serde(default)]
            launcher: OpenLauncher,
            #[serde(default)]
            actions: Vec<OpenAction>,
        }

        let value = OpenConfigDe::deserialize(deserializer)?;
        Ok(Self { launcher: value.launcher, actions: value.actions })
    }
}

impl SerdeReplace for OpenConfig {
    fn replace(&mut self, value: toml::Value) -> Result<(), Box<dyn Error>> {
        let toml::Value::Table(table) = value else {
            *self = Self::deserialize(value)?;
            return Ok(());
        };

        for (key, value) in table {
            match key.as_str() {
                "launcher" => self.launcher.replace(value)?,
                "actions" => self.actions = Vec::<OpenAction>::deserialize(value)?,
                _ => return Err(format!("Unrecognized open field: {key}").into()),
            }
        }

        Ok(())
    }
}

impl Default for OpenConfig {
    fn default() -> Self {
        Self { launcher: OpenLauncher::Default, actions: Vec::new() }
    }
}

impl OpenConfig {
    /// Resolve a URL-like target to a command.
    pub fn resolve(&self, target: &str) -> Option<ResolvedOpen> {
        let target = OpenTarget::new(target)?;
        self.actions
            .iter()
            .find(|action| action.matches(&target))
            .map(|action| action.command(&target))
            .or_else(|| Some(self.launcher.command(&target)))
    }
}

/// Fallback URL launcher.
#[derive(Default, Clone, Debug, PartialEq, Eq)]
pub enum OpenLauncher {
    #[default]
    Default,
    Command(Program),
}

impl Serialize for OpenLauncher {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Default => serializer.serialize_str("Default"),
            Self::Command(program) => program.serialize(serializer),
        }
    }
}

impl OpenLauncher {
    fn command(&self, target: &OpenTarget<'_>) -> ResolvedOpen {
        match self {
            Self::Default => platform_default_launcher(target),
            Self::Command(program) => {
                command_with_target(OpenActionMode::External, program, target)
            },
        }
    }
}

impl<'de> Deserialize<'de> for OpenLauncher {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct OpenLauncherVisitor;

        impl<'a> Visitor<'a> for OpenLauncherVisitor {
            type Value = OpenLauncher;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("\"Default\" or a command")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                if value == "Default" {
                    Ok(OpenLauncher::Default)
                } else {
                    Ok(OpenLauncher::Command(Program::Just(value.into())))
                }
            }

            fn visit_map<M>(self, map: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'a>,
            {
                Program::deserialize(de::value::MapAccessDeserializer::new(map))
                    .map(OpenLauncher::Command)
            }
        }

        deserializer.deserialize_any(OpenLauncherVisitor)
    }
}

impl SerdeReplace for OpenLauncher {
    fn replace(&mut self, value: toml::Value) -> Result<(), Box<dyn Error>> {
        *self = Self::deserialize(value)?;
        Ok(())
    }
}

/// URL-specific open action.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenAction {
    protocols: Option<Vec<String>>,
    url: Option<UrlPattern>,
    extensions: Option<Vec<String>>,
    mode: OpenActionMode,
    command: Program,
}

/// Destination used for matching URL open actions.
#[derive(Serialize, Deserialize, Default, Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenActionMode {
    #[default]
    External,
    Tab,
    Window,
}

impl Serialize for OpenAction {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut fields = 2;
        fields += usize::from(self.protocols.is_some());
        fields += usize::from(self.url.is_some());
        fields += usize::from(self.extensions.is_some());

        let mut state = serializer.serialize_struct("OpenAction", fields)?;
        if let Some(protocols) = &self.protocols {
            state.serialize_field("protocol", protocols)?;
        }
        if let Some(url) = &self.url {
            state.serialize_field("url", url)?;
        }
        if let Some(extensions) = &self.extensions {
            state.serialize_field("extension", extensions)?;
        }
        state.serialize_field("mode", &self.mode)?;
        state.serialize_field("command", &self.command)?;
        state.end()
    }
}

impl OpenAction {
    fn matches(&self, target: &OpenTarget<'_>) -> bool {
        if let Some(protocols) = &self.protocols
            && !target
                .protocol
                .as_ref()
                .is_some_and(|protocol| protocols.iter().any(|p| p == protocol))
        {
            return false;
        }

        if let Some(extensions) = &self.extensions
            && !target
                .extension
                .as_ref()
                .is_some_and(|extension| extensions.iter().any(|e| e == extension))
        {
            return false;
        }

        if let Some(url) = &self.url {
            return url.is_match(target.text);
        }

        true
    }

    fn command(&self, target: &OpenTarget<'_>) -> ResolvedOpen {
        command_with_target(self.mode, &self.command, target)
    }
}

impl<'de> Deserialize<'de> for OpenAction {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct OpenActionDe {
            #[serde(default, alias = "scheme")]
            protocol: Option<StringList>,
            #[serde(default)]
            url: Option<UrlPattern>,
            #[serde(default, alias = "ext")]
            extension: Option<StringList>,
            #[serde(default)]
            mode: OpenActionMode,
            command: Program,
        }

        let value = OpenActionDe::deserialize(deserializer)?;
        if value.protocol.is_none() && value.url.is_none() && value.extension.is_none() {
            return Err(D::Error::custom(
                "open actions require at least one of protocol, url, or extension",
            ));
        }

        let protocols =
            value.protocol.map(|protocols| protocols.lowercase::<D::Error>()).transpose()?;
        let extensions =
            value.extension.map(|extensions| extensions.lowercase::<D::Error>()).transpose()?;

        if protocols
            .as_ref()
            .is_some_and(|protocols| protocols.iter().any(|protocol| !valid_protocol(protocol)))
        {
            return Err(D::Error::custom("protocol matchers must be valid URL schemes"));
        }

        if extensions.as_ref().is_some_and(|extensions| {
            extensions.iter().any(|extension| {
                extension.chars().any(char::is_control)
                    || extension.chars().any(|c| matches!(c, '.' | '/' | '\\' | ':' | '?' | '#'))
            })
        }) {
            return Err(D::Error::custom(
                "extension matchers must be file extensions without dots or separators",
            ));
        }

        Ok(Self { protocols, url: value.url, extensions, mode: value.mode, command: value.command })
    }
}

/// Resolved command for opening a target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedOpen {
    pub mode: OpenActionMode,
    pub program: String,
    pub args: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct OpenTarget<'a> {
    text: &'a str,
    protocol: Option<String>,
    extension: Option<String>,
}

impl<'a> OpenTarget<'a> {
    fn new(text: &'a str) -> Option<Self> {
        if text.is_empty() || text.len() > MAX_OPEN_TARGET_LEN || text.chars().any(char::is_control)
        {
            return None;
        }

        let protocol = protocol(text);
        let extension = extension(text);
        Some(Self { text, protocol, extension })
    }
}

fn protocol(text: &str) -> Option<String> {
    let (protocol, _) = text.split_once(':')?;
    valid_protocol(protocol).then(|| protocol.to_ascii_lowercase())
}

fn valid_protocol(protocol: &str) -> bool {
    let mut chars = protocol.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_alphabetic()
        || !chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    {
        return false;
    }

    true
}

fn extension(text: &str) -> Option<String> {
    let without_query = text.split(['?', '#']).next().unwrap_or(text);
    let path = path_part(without_query);
    let file_name = path.rsplit('/').next().unwrap_or(path);
    let (_, extension) = file_name.rsplit_once('.')?;
    if extension.is_empty() { None } else { Some(extension.to_ascii_lowercase()) }
}

fn path_part(text: &str) -> &str {
    match text.split_once(':') {
        Some((_, after_scheme)) => match after_scheme.strip_prefix("//") {
            Some(authority_and_path) => {
                authority_and_path.find('/').map(|index| &authority_and_path[index..]).unwrap_or("")
            },
            None => after_scheme,
        },
        None => text,
    }
}

fn command_with_target(
    mode: OpenActionMode,
    program: &Program,
    target: &OpenTarget<'_>,
) -> ResolvedOpen {
    let mut args = program.args().to_vec();
    args.push(target.text.into());
    ResolvedOpen { mode, program: program.program().into(), args }
}

#[cfg(not(any(target_os = "macos", windows)))]
fn platform_default_launcher(target: &OpenTarget<'_>) -> ResolvedOpen {
    ResolvedOpen {
        mode: OpenActionMode::External,
        program: "xdg-open".into(),
        args: vec![target.text.into()],
    }
}

#[cfg(target_os = "macos")]
fn platform_default_launcher(target: &OpenTarget<'_>) -> ResolvedOpen {
    ResolvedOpen {
        mode: OpenActionMode::External,
        program: "open".into(),
        args: vec![target.text.into()],
    }
}

#[cfg(windows)]
fn platform_default_launcher(target: &OpenTarget<'_>) -> ResolvedOpen {
    ResolvedOpen {
        mode: OpenActionMode::External,
        program: "cmd".into(),
        args: vec!["/c".into(), "start".into(), "".into(), target.text.into()],
    }
}

#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
struct StringList(Vec<String>);

impl StringList {
    fn lowercase<E>(self) -> Result<Vec<String>, E>
    where
        E: de::Error,
    {
        if self.0.is_empty() {
            return Err(E::custom("list must not be empty"));
        }

        self.0
            .into_iter()
            .map(|value| {
                if value.is_empty() {
                    Err(E::custom("list entries must not be empty"))
                } else {
                    Ok(value.to_ascii_lowercase())
                }
            })
            .collect()
    }
}

impl<'de> Deserialize<'de> for StringList {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum OneOrMany {
            One(String),
            Many(Vec<String>),
        }

        match OneOrMany::deserialize(deserializer)? {
            OneOrMany::One(value) => Ok(Self(vec![value])),
            OneOrMany::Many(values) => Ok(Self(values)),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct UrlPattern(String);

impl UrlPattern {
    fn is_match(&self, text: &str) -> bool {
        regex_automata::meta::Regex::new(&self.0).is_ok_and(|regex| regex.is_match(text))
    }
}

impl Serialize for UrlPattern {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for UrlPattern {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let pattern = String::deserialize(deserializer)?;
        if pattern.is_empty() {
            return Err(D::Error::custom("url pattern must not be empty"));
        }

        regex_automata::meta::Regex::new(&pattern).map_err(D::Error::custom)?;
        Ok(Self(pattern))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::UiConfig;

    #[test]
    fn default_launcher_resolves_to_platform_command() {
        let resolved = OpenConfig::default().resolve("https://example.org").unwrap();

        #[cfg(not(any(target_os = "macos", windows)))]
        assert_eq!(resolved, ResolvedOpen {
            mode: OpenActionMode::External,
            program: "xdg-open".into(),
            args: vec!["https://example.org".into()],
        });

        #[cfg(target_os = "macos")]
        assert_eq!(resolved, ResolvedOpen {
            mode: OpenActionMode::External,
            program: "open".into(),
            args: vec!["https://example.org".into()],
        });

        #[cfg(windows)]
        assert_eq!(resolved, ResolvedOpen {
            mode: OpenActionMode::External,
            program: "cmd".into(),
            args: vec!["/c".into(), "start".into(), "".into(), "https://example.org".into()],
        });
    }

    #[test]
    fn first_matching_open_action_wins() {
        let config = toml::from_str::<OpenConfig>(
            r#"
            [[actions]]
            protocol = ["https"]
            mode = "Tab"
            command = "first"

            [[actions]]
            protocol = ["https"]
            mode = "Window"
            command = "second"
            "#,
        )
        .unwrap();

        let resolved = config.resolve("https://example.org").unwrap();
        assert_eq!(resolved.program, "first");
        assert_eq!(resolved.mode, OpenActionMode::Tab);
    }

    #[test]
    fn open_action_modes_are_resolved() {
        for (mode, expected) in [
            (None, OpenActionMode::External),
            (Some("External"), OpenActionMode::External),
            (Some("Tab"), OpenActionMode::Tab),
            (Some("Window"), OpenActionMode::Window),
        ] {
            let mode = mode.map(|mode| format!("mode = \"{mode}\"\n")).unwrap_or_default();
            let config = toml::from_str::<OpenConfig>(&format!(
                r#"
                [[actions]]
                protocol = "https"
                {mode}
                command = {{ program = "browser", args = ["--new-tab"] }}
                "#
            ))
            .unwrap();

            let resolved = config.resolve("https://example.org").unwrap();
            assert_eq!(resolved.mode, expected);
            assert_eq!(resolved.args, vec![
                "--new-tab".to_owned(),
                "https://example.org".to_owned()
            ]);
        }
    }

    #[test]
    fn all_open_action_matchers_must_match() {
        let config = toml::from_str::<OpenConfig>(
            r#"
            [[actions]]
            protocol = ["https"]
            extension = ["pdf"]
            command = "zathura"
            "#,
        )
        .unwrap();

        assert_eq!(config.resolve("https://example.org/file.pdf").unwrap().program, "zathura");
        assert_ne!(config.resolve("https://example.org/file.txt").unwrap().program, "zathura");
    }

    #[test]
    fn open_actions_match_protocol_case_insensitively() {
        let config = toml::from_str::<OpenConfig>(
            r#"
            [[actions]]
            protocol = "HTTPS"
            command = "browser"
            "#,
        )
        .unwrap();

        assert_eq!(config.resolve("https://example.org").unwrap().program, "browser");
    }

    #[test]
    fn open_actions_match_url_regex() {
        let config = toml::from_str::<OpenConfig>(
            r#"
            [[actions]]
            url = "github\\.com"
            command = { program = "firefox", args = ["--new-tab"] }
            "#,
        )
        .unwrap();

        assert_eq!(
            config.resolve("https://github.com/alacritty/alacritty").unwrap(),
            ResolvedOpen {
                mode: OpenActionMode::External,
                program: "firefox".into(),
                args: vec!["--new-tab".into(), "https://github.com/alacritty/alacritty".into()],
            }
        );
    }

    #[test]
    fn open_actions_match_path_extension() {
        let config = toml::from_str::<OpenConfig>(
            r#"
            [[actions]]
            extension = ["pdf"]
            command = "zathura"
            "#,
        )
        .unwrap();

        assert_eq!(config.resolve("file:///tmp/report.pdf").unwrap().program, "zathura");
        assert_eq!(
            config.resolve("https://example.org/report.pdf?download=1#page=2").unwrap().program,
            "zathura"
        );
        assert_ne!(config.resolve("https://example.pdf/download").unwrap().program, "zathura");
    }

    #[test]
    fn target_is_passed_as_single_argument() {
        let config = toml::from_str::<OpenConfig>(
            r#"
            [[actions]]
            protocol = "https"
            command = { program = "browser", args = ["--new-tab"] }
            "#,
        )
        .unwrap();

        assert_eq!(config.resolve("https://example.org/a;rm -rf /").unwrap(), ResolvedOpen {
            mode: OpenActionMode::External,
            program: "browser".into(),
            args: vec!["--new-tab".into(), "https://example.org/a;rm -rf /".into()],
        });
    }

    #[test]
    fn replacing_launcher_preserves_actions() {
        let mut config = toml::from_str::<OpenConfig>(
            r#"
            launcher = "fallback"

            [[actions]]
            protocol = "https"
            command = "browser"
            "#,
        )
        .unwrap();

        config
            .replace(
                toml::from_str(
                    r#"
                [launcher]
                program = "custom-open"
                args = ["--reuse-window"]
                "#,
                )
                .unwrap(),
            )
            .unwrap();

        assert_eq!(config.resolve("https://example.org").unwrap().program, "browser");
        assert_eq!(config.resolve("file:/tmp/readme").unwrap(), ResolvedOpen {
            mode: OpenActionMode::External,
            program: "custom-open".into(),
            args: vec!["--reuse-window".into(), "file:/tmp/readme".into()],
        });
    }

    #[test]
    fn launcher_command_serializes_as_program_shape() {
        let launcher = OpenLauncher::Command(Program::Just("browser".into()));
        let serialized = toml::Value::try_from(launcher).unwrap();
        assert_eq!(serialized, toml::Value::String("browser".into()));

        let launcher = OpenLauncher::Command(Program::WithArgs {
            program: "browser".into(),
            args: vec!["--new-tab".into()],
        });
        let serialized = toml::Value::try_from(launcher).unwrap();

        let table = serialized.as_table().unwrap();
        assert_eq!(table.get("program").and_then(toml::Value::as_str), Some("browser"));
        assert_eq!(table.get("Command"), None);
    }

    #[test]
    fn open_action_serializes_documented_matcher_names() {
        let action = OpenAction {
            protocols: Some(vec!["https".into()]),
            url: Some(UrlPattern("example".into())),
            extensions: Some(vec!["pdf".into()]),
            mode: OpenActionMode::Tab,
            command: Program::Just("browser".into()),
        };

        let serialized = toml::Value::try_from(action).unwrap();
        let table = serialized.as_table().unwrap();

        assert!(table.contains_key("protocol"));
        assert!(table.contains_key("extension"));
        assert_eq!(table.get("mode").and_then(toml::Value::as_str), Some("Tab"));
        assert!(!table.contains_key("protocols"));
        assert!(!table.contains_key("extensions"));

        let decoded = OpenAction::deserialize(serialized).unwrap();
        assert_eq!(decoded.mode, OpenActionMode::Tab);
    }

    #[test]
    fn invalid_open_targets_are_rejected() {
        let config = OpenConfig::default();

        assert_eq!(config.resolve(""), None);
        assert_eq!(config.resolve("https://example.org/\n"), None);
    }

    #[test]
    fn invalid_open_action_config_fails() {
        assert!(
            toml::from_str::<OpenConfig>(
                r#"
            [[actions]]
            command = "browser"
            "#,
            )
            .is_err()
        );

        assert!(
            toml::from_str::<OpenConfig>(
                r#"
            [[actions]]
            url = "["
            command = "browser"
            "#,
            )
            .is_err()
        );

        assert!(
            toml::from_str::<OpenConfig>(
                r#"
            [[actions]]
            protocol = []
            command = "browser"
            "#,
            )
            .is_err()
        );

        assert!(
            toml::from_str::<OpenConfig>(
                r#"
            [[actions]]
            protocol = "https://"
            command = "browser"
            "#,
            )
            .is_err()
        );

        assert!(
            toml::from_str::<OpenConfig>(
                r#"
            [[actions]]
            url = ""
            command = "browser"
            "#,
            )
            .is_err()
        );

        assert!(
            toml::from_str::<OpenConfig>(
                r#"
            [[actions]]
            extension = "foo\\bar"
            command = "browser"
            "#,
            )
            .is_err()
        );

        assert!(
            toml::from_str::<OpenConfig>(
                r#"
            [[actions]]
            protocol = "https"
            mode = "Split"
            command = "browser"
            "#,
            )
            .is_err()
        );
    }

    #[test]
    fn open_config_parses_as_part_of_ui_config() {
        let config = toml::from_str::<UiConfig>(
            r#"
            [open]
            launcher = "fallback"

            [[open.actions]]
            protocol = "https"
            command = { program = "browser", args = ["--new-tab"] }
            "#,
        )
        .unwrap();

        assert_eq!(config.open.resolve("https://example.org").unwrap(), ResolvedOpen {
            mode: OpenActionMode::External,
            program: "browser".into(),
            args: vec!["--new-tab".into(), "https://example.org".into()],
        });
        assert_eq!(config.open.resolve("file:/tmp/readme").unwrap().program, "fallback");
    }
}
