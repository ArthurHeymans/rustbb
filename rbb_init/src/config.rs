use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::io;

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    #[default]
    Longrun,
    Oneshot,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum Restart {
    #[default]
    Always,
    OnFailure,
    Never,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Service {
    pub name: String,
    pub command: Vec<String>,
    #[serde(default)]
    pub kind: Kind,
    #[serde(default)]
    pub after: Vec<String>,
    pub restart: Option<Restart>,
    #[serde(default = "restart_delay")]
    pub restart_delay_ms: u64,
    #[serde(default)]
    pub disabled: bool,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub cwd: Option<String>,
}

fn restart_delay() -> u64 {
    1000
}
fn shutdown_timeout() -> u64 {
    5000
}
fn control_path() -> String {
    "/run/rbb/control".into()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "control_path")]
    pub control: String,
    #[serde(default = "shutdown_timeout")]
    pub shutdown_timeout_ms: u64,
    #[serde(default)]
    pub services: Vec<Service>,
    /// Optional real disk root; kernel root= parameters can override this.
    pub root: Option<rbb_boot::RootConfig>,
    pub boot: Option<rbb_boot::model::BootConfig>,
    #[serde(default)]
    pub modules: Vec<String>,
}

impl Service {
    pub fn policy(&self) -> Restart {
        self.restart.unwrap_or(if self.kind == Kind::Oneshot {
            Restart::Never
        } else {
            Restart::Always
        })
    }
}

impl Config {
    pub fn parse(text: &str) -> io::Result<Self> {
        let config: Self = toml::from_str(text).map_err(io::Error::other)?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> io::Result<()> {
        let invalid = |message: String| io::Error::new(io::ErrorKind::InvalidInput, message);
        if self.shutdown_timeout_ms > 300_000 {
            return Err(invalid("shutdown timeout must not exceed 300000ms".into()));
        }
        if !self.control.starts_with('/') || self.control.len() > 100 {
            return Err(invalid(
                "control socket must be an absolute path <= 100 bytes".into(),
            ));
        }
        if self.root.is_some() && self.boot.is_some() {
            return Err(invalid("root and boot are mutually exclusive".into()));
        }
        if let Some(root) = &self.root {
            root.validate()?;
        }
        if let Some(boot) = &self.boot {
            boot.validate()?;
        }
        if self
            .modules
            .iter()
            .any(|m| m.is_empty() || m.contains(['\0', ' ', '/']))
        {
            return Err(invalid(
                "modules must be names or modaliases, not paths/parameters".into(),
            ));
        }
        let mut names = BTreeSet::new();
        for service in &self.services {
            if service.name.is_empty()
                || !service
                    .name
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
                || !names.insert(service.name.as_str())
            {
                return Err(invalid(format!(
                    "invalid or duplicate service: {}",
                    service.name
                )));
            }
            if service.command.is_empty() || !service.command[0].starts_with('/') {
                return Err(invalid(format!(
                    "{}: command must start with an absolute executable path",
                    service.name
                )));
            }
            if service.restart_delay_ms == 0 || service.restart_delay_ms > 86_400_000 {
                return Err(invalid(format!(
                    "{}: restart delay must be 1..86400000ms",
                    service.name
                )));
            }
            if service.command.iter().any(|a| a.contains('\0'))
                || service
                    .env
                    .iter()
                    .any(|(k, v)| k.is_empty() || k.contains(['=', '\0']) || v.contains('\0'))
                || service
                    .cwd
                    .as_ref()
                    .is_some_and(|v| !v.starts_with('/') || v.contains('\0'))
            {
                return Err(invalid(format!(
                    "{}: invalid command, environment or cwd",
                    service.name
                )));
            }
        }
        // Kahn's algorithm also provides a stable dependency-first startup order.
        self.order()?;
        Ok(())
    }

    pub fn order(&self) -> io::Result<Vec<usize>> {
        let mut ordered = Vec::new();
        let mut done = BTreeSet::new();
        while ordered.len() < self.services.len() {
            let before = ordered.len();
            for (i, service) in self.services.iter().enumerate() {
                if done.contains(service.name.as_str()) {
                    continue;
                }
                for dep in &service.after {
                    if !self.services.iter().any(|s| &s.name == dep) {
                        return Err(io::Error::other(format!(
                            "{}: unknown dependency {dep}",
                            service.name
                        )));
                    }
                }
                if service.after.iter().all(|d| done.contains(d.as_str())) {
                    ordered.push(i);
                    done.insert(service.name.as_str());
                }
            }
            if before == ordered.len() {
                return Err(io::Error::other("service dependency cycle"));
            }
        }
        Ok(ordered)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dependency_order_and_oneshot_default() {
        let c = Config::parse(
            r#"
            [[services]]
            name = "daemon"
            command = ["/bin/daemon"]
            after = ["setup"]
            [[services]]
            name = "setup"
            command = ["/bin/setup"]
            kind = "oneshot"
        "#,
        )
        .unwrap();
        assert_eq!(c.order().unwrap(), [1, 0]);
        assert_eq!(c.services[1].policy(), Restart::Never);
    }

    #[test]
    fn rejects_bad_graphs_and_commands() {
        for text in [
            "[[services]]\nname='x'\ncommand=[]",
            "[[services]]\nname='x'\ncommand=['relative']",
            "[[services]]\nname='x'\ncommand=['/x']\nafter=['missing']",
            "[[services]]\nname='x'\ncommand=['/x']\nafter=['x']",
            "[[services]]\nname='x'\ncommand=['/x']\nrestart_delay_ms=0",
            "[[services]]\nname='x'\ncommand=['/x']\n[[services]]\nname='x'\ncommand=['/y']",
            "[[services]]\nname='x'\ncommand=['/x']\ncommnad=['typo']",
        ] {
            assert!(Config::parse(text).is_err(), "accepted {text}");
        }
    }
}
