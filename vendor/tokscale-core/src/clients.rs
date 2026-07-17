#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathRoot {
    Home,
    XdgData,
    Config,
    EnvVar {
        var: &'static str,
        fallback_relative: &'static str,
    },
}

impl PathRoot {
    pub fn resolve_with_env_strategy(&self, home_dir: &str, use_env_roots: bool) -> String {
        match self {
            PathRoot::Home => home_dir.to_string(),
            PathRoot::XdgData => {
                if use_env_roots {
                    std::env::var("XDG_DATA_HOME")
                        .unwrap_or_else(|_| format!("{}/.local/share", home_dir))
                } else {
                    format!("{}/.local/share", home_dir)
                }
            }
            PathRoot::Config => {
                if use_env_roots {
                    if let Some(custom) = std::env::var_os("TOKSCALE_CONFIG_DIR") {
                        if !custom.is_empty() {
                            return custom.to_string_lossy().into_owned();
                        }
                    }

                    #[cfg(target_os = "linux")]
                    if let Ok(xdg_config_home) = std::env::var("XDG_CONFIG_HOME") {
                        return format!("{xdg_config_home}/tokscale");
                    }

                    // Match paths::get_config_dir() so default Windows scans
                    // read the same %APPDATA% root used by cache writers.
                    #[cfg(target_os = "windows")]
                    if let Some(dir) = dirs::config_dir() {
                        return dir.join("tokscale").to_string_lossy().into_owned();
                    }
                }

                format!("{home_dir}/.config/tokscale")
            }
            PathRoot::EnvVar {
                var,
                fallback_relative,
            } => {
                if use_env_roots {
                    let val = std::env::var(var).unwrap_or_default();
                    if val.trim().is_empty() {
                        format!("{}/{}", home_dir, fallback_relative)
                    } else {
                        val
                    }
                } else {
                    format!("{}/{}", home_dir, fallback_relative)
                }
            }
        }
    }

    pub fn resolve(&self, home_dir: &str) -> String {
        self.resolve_with_env_strategy(home_dir, true)
    }
}

#[derive(Debug, Clone)]
pub struct ClientDef {
    pub id: &'static str,
    pub root: PathRoot,
    pub relative_path: &'static str,
    pub pattern: &'static str,
    pub headless: bool,
    pub parse_local: bool,
    pub submit_default: bool,
}

impl ClientDef {
    pub fn resolve_path_with_env_strategy(&self, home_dir: &str, use_env_roots: bool) -> String {
        format!(
            "{}/{}",
            self.root.resolve_with_env_strategy(home_dir, use_env_roots),
            self.relative_path
        )
    }

    pub fn resolve_path(&self, home_dir: &str) -> String {
        self.resolve_path_with_env_strategy(home_dir, true)
    }
}

macro_rules! define_clients {
    ( $( $variant:ident = $index:expr => { id: $id:expr, root: $root:expr, relative: $rel:expr, pattern: $pat:expr, headless: $hl:expr, parse_local: $pl:expr, submit_default: $sd:expr } ),+ $(,)? ) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        #[repr(usize)]
        pub enum ClientId {
            $( $variant = $index ),+
        }

        impl ClientId {
            pub const COUNT: usize = [ $( $index ),+ ].len();
            pub const ALL: [ClientId; Self::COUNT] = [ $( ClientId::$variant ),+ ];

            pub fn data(&self) -> &'static ClientDef {
                &CLIENTS[*self as usize]
            }

            pub fn as_str(&self) -> &'static str {
                self.data().id
            }

            pub fn file_pattern(&self) -> &'static str {
                self.data().pattern
            }

            pub fn supports_headless(&self) -> bool {
                self.data().headless
            }

            pub fn parse_local(&self) -> bool {
                self.data().parse_local
            }

            pub fn submit_default(&self) -> bool {
                self.data().submit_default
            }

            pub fn iter() -> impl Iterator<Item = ClientId> {
                Self::ALL.iter().copied()
            }

            #[allow(clippy::should_implement_trait)]
            pub fn from_str(s: &str) -> Option<ClientId> {
                Self::ALL.iter().copied().find(|c| c.as_str() == s)
            }
        }

        pub const CLIENTS: [ClientDef; ClientId::COUNT] = [
            $( ClientDef {
                id: $id,
                root: $root,
                relative_path: $rel,
                pattern: $pat,
                headless: $hl,
                parse_local: $pl,
                submit_default: $sd,
            } ),+
        ];

        const _: () = {
            let mut i = 0;
            $(
                assert!($index == i, "ClientId indices must be sequential");
                i += 1;
                let _ = i;
            )+
        };
    };
}

define_clients!(
    OpenCode = 0 => {
        id: "opencode",
        root: PathRoot::XdgData,
        relative: "opencode/storage/message",
        pattern: "*.json",
        headless: false,
        parse_local: true,
        submit_default: true
    },
    Claude = 1 => {
        id: "claude",
        root: PathRoot::Home,
        relative: ".claude/projects",
        pattern: "*.jsonl",
        headless: false,
        parse_local: true,
        submit_default: true
    },
    Codex = 2 => {
        id: "codex",
        root: PathRoot::EnvVar {
            var: "CODEX_HOME",
            fallback_relative: ".codex",
        },
        relative: "sessions",
        pattern: "*.jsonl",
        headless: true,
        parse_local: true,
        submit_default: true
    },
    Cursor = 3 => {
        id: "cursor",
        root: PathRoot::Home,
        relative: ".config/tokscale/cursor-cache",
        pattern: "usage*.csv",
        headless: false,
        parse_local: false,
        submit_default: true
    },
    Gemini = 4 => {
        id: "gemini",
        root: PathRoot::EnvVar {
            var: "GEMINI_CLI_HOME",
            fallback_relative: ".gemini",
        },
        relative: "tmp",
        pattern: "*.json|*.jsonl",
        headless: false,
        parse_local: true,
        submit_default: true
    },
    Amp = 5 => {
        id: "amp",
        root: PathRoot::XdgData,
        relative: "amp/threads",
        pattern: "T-*.json",
        headless: false,
        parse_local: true,
        submit_default: true
    },
    Droid = 6 => {
        id: "droid",
        root: PathRoot::Home,
        relative: ".factory/sessions",
        pattern: "*.settings.json",
        headless: false,
        parse_local: true,
        submit_default: true
    },
    OpenClaw = 7 => {
        id: "openclaw",
        root: PathRoot::Home,
        relative: ".openclaw/agents",
        pattern: "*.jsonl*",
        headless: false,
        parse_local: true,
        submit_default: true
    },
    Pi = 8 => {
        id: "pi",
        root: PathRoot::Home,
        relative: ".pi/agent/sessions",
        pattern: "*.jsonl",
        headless: false,
        parse_local: true,
        submit_default: true
    },
    Kimi = 9 => {
        id: "kimi",
        root: PathRoot::Home,
        relative: ".kimi/sessions",
        pattern: "wire.jsonl",
        headless: false,
        parse_local: true,
        submit_default: true
    },
    Qwen = 10 => {
        id: "qwen",
        root: PathRoot::Home,
        relative: ".qwen/projects",
        pattern: "*.jsonl",
        headless: false,
        parse_local: true,
        submit_default: true
    },
    RooCode = 11 => {
        id: "roocode",
        root: PathRoot::Home,
        relative: ".config/Code/User/globalStorage/rooveterinaryinc.roo-cline/tasks",
        pattern: "ui_messages.json",
        headless: false,
        parse_local: true,
        submit_default: true
    },
    KiloCode = 12 => {
        id: "kilocode",
        root: PathRoot::Home,
        relative: ".config/Code/User/globalStorage/kilocode.kilo-code/tasks",
        pattern: "ui_messages.json",
        headless: false,
        parse_local: true,
        submit_default: true
    },
    Mux = 13 => {
        id: "mux",
        root: PathRoot::Home,
        relative: ".mux/sessions",
        pattern: "session-usage.json",
        headless: false,
        parse_local: true,
        submit_default: true
    },
    Kilo = 14 => {
        id: "kilo",
        root: PathRoot::XdgData,
        relative: "kilo/kilo.db",
        pattern: "kilo.db",
        headless: false,
        parse_local: true,
        submit_default: true
    },
    Crush = 15 => {
        id: "crush",
        root: PathRoot::XdgData,
        relative: "crush/projects.json",
        pattern: "projects.json",
        headless: false,
        parse_local: true,
        submit_default: false
    },
    Hermes = 16 => {
        id: "hermes",
        root: PathRoot::EnvVar {
            var: "HERMES_HOME",
            fallback_relative: ".hermes",
        },
        relative: "state.db",
        pattern: "state.db",
        headless: false,
        parse_local: true,
        submit_default: true
    },
    Copilot = 17 => {
        id: "copilot",
        root: PathRoot::Home,
        relative: ".copilot/otel",
        pattern: "*.jsonl",
        headless: false,
        parse_local: true,
        submit_default: true
    },
    Goose = 18 => {
        id: "goose",
        root: PathRoot::XdgData,
        relative: "goose/sessions/sessions.db",
        pattern: "sessions.db",
        headless: false,
        parse_local: true,
        submit_default: true
    },
    Codebuff = 19 => {
        id: "codebuff",
        root: PathRoot::EnvVar {
            var: "CODEBUFF_DATA_DIR",
            fallback_relative: ".config/manicode",
        },
        relative: "projects",
        pattern: "chat-messages.json",
        headless: false,
        parse_local: true,
        submit_default: true
    },
    Antigravity = 20 => {
        id: "antigravity",
        root: PathRoot::Config,
        relative: "antigravity-cache/sessions",
        pattern: "*.jsonl",
        headless: false,
        parse_local: true,
        submit_default: false
    },
    Zed = 21 => {
        id: "zed",
        root: PathRoot::XdgData,
        relative: "zed/threads/threads.db",
        pattern: "threads.db",
        headless: false,
        parse_local: true,
        submit_default: true
    },
    Kiro = 22 => {
        id: "kiro",
        root: PathRoot::Home,
        relative: ".kiro/sessions/cli",
        pattern: "*.json",
        headless: false,
        parse_local: true,
        submit_default: true
    },
    Trae = 23 => {
        id: "trae",
        root: PathRoot::Config,
        relative: "trae-cache/sessions",
        pattern: "*.json",
        headless: false,
        parse_local: true,
        submit_default: false
    },
    Warp = 24 => {
        id: "warp",
        root: PathRoot::Config,
        relative: "warp-cache",
        pattern: "usage*.json",
        headless: false,
        parse_local: false,
        submit_default: false
    },
    Cline = 25 => {
        id: "cline",
        root: PathRoot::Home,
        relative: ".config/Code/User/globalStorage/saoudrizwan.claude-dev/tasks",
        pattern: "ui_messages.json",
        headless: false,
        parse_local: true,
        submit_default: true
    },
    // Antigravity CLI stores each conversation as a SQLite `.db` under
    // `~/.gemini/antigravity-cli/conversations/`. Unlike the IDE-backed
    // `Antigravity` client (which pulls usage from a running language server
    // over RPC and caches JSONL under the config dir), the CLI usage sits on
    // disk and is read directly — no RPC, no `antigravity sync` needed. Honors
    // `GEMINI_CLI_HOME` so a relocated Gemini home is picked up.
    AntigravityCli = 26 => {
        id: "antigravity-cli",
        root: PathRoot::EnvVar {
            var: "GEMINI_CLI_HOME",
            fallback_relative: ".gemini",
        },
        relative: "antigravity-cli/conversations",
        pattern: "*.db",
        headless: false,
        parse_local: true,
        submit_default: true
    },
    // jcode stores each session as a compact JSON snapshot at
    // `~/.jcode/sessions/session_*.json` (honors `JCODE_HOME`) plus a sibling
    // `<session>.journal.jsonl` append-log of newer turns. The scanner gains a
    // `session_*.json` arm; the lane fingerprints snapshot + journal
    // (from_jcode_path) and is exempt from mtime pruning so a journal-only
    // append is never lost. Per-message `token_usage` is authoritative.
    Jcode = 27 => {
        id: "jcode",
        root: PathRoot::EnvVar {
            var: "JCODE_HOME",
            fallback_relative: ".jcode",
        },
        relative: "sessions",
        pattern: "session_*.json",
        headless: false,
        parse_local: true,
        submit_default: true
    },
    // MiMo Code (Xiaomi) stores sessions as WAL-mode SQLite under
    // `$XDG_DATA_HOME/micode/*.db`, with authoritative per-message tokens,
    // cost, and agent. Read directly via the generic `*.db` glob; exempt from
    // mtime pruning + `-wal`-probed in `latest_source_mtime_ms` (see lib.rs).
    MiMoCode = 28 => {
        id: "micode",
        root: PathRoot::XdgData,
        // Real MiMo Code installs write to ~/.local/share/mimocode, not /micode
        // (#784) — the old path found nothing, so MiMo usage read as empty.
        relative: "mimocode",
        pattern: "*.db",
        headless: false,
        parse_local: true,
        submit_default: true
    },
    // gjc (gajae-code) stores sessions as JSONL under
    // `$GJC_CODING_AGENT_DIR/sessions/*.jsonl` (default `~/.gjc/agent`), each
    // assistant message carrying an authoritative `usage.cost.total` (USD).
    // The lane reprices only when that cost is absent (A1 guard, see lib.rs).
    Gjc = 29 => {
        id: "gjc",
        root: PathRoot::EnvVar {
            var: "GJC_CODING_AGENT_DIR",
            fallback_relative: ".gjc/agent",
        },
        relative: "sessions",
        pattern: "*.jsonl",
        headless: false,
        parse_local: true,
        submit_default: true
    },
    // Grok Build stores ACP session updates under
    // `$GROK_HOME/sessions/<urlencoded-workspace>/<session-id>/updates.jsonl`
    // (default `~/.grok`). Cumulative `totalTokens` deltas are recorded as
    // input tokens; sibling `signals.json` reconciles compaction undercount.
    // Upstream numbers this client 27 — intervening clients differ in our
    // vendor numbering.
    Grok = 30 => {
        id: "grok",
        root: PathRoot::EnvVar {
            var: "GROK_HOME",
            fallback_relative: ".grok",
        },
        relative: "sessions",
        pattern: "updates.jsonl",
        headless: false,
        parse_local: true,
        submit_default: true
    }
);

pub struct ClientCounts {
    counts: [i32; ClientId::COUNT],
}

impl ClientCounts {
    pub fn new() -> Self {
        Self {
            counts: [0; ClientId::COUNT],
        }
    }

    pub fn get(&self, client: ClientId) -> i32 {
        self.counts[client as usize]
    }

    pub fn set(&mut self, client: ClientId, value: i32) {
        self.counts[client as usize] = value;
    }

    pub fn add(&mut self, client: ClientId, value: i32) {
        self.counts[client as usize] += value;
    }
}

impl Default for ClientCounts {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::ffi::OsStr;

    struct EnvGuard(Vec<(&'static str, Option<std::ffi::OsString>)>);

    impl EnvGuard {
        fn capture(keys: &[&'static str]) -> Self {
            Self(
                keys.iter()
                    .map(|key| (*key, std::env::var_os(key)))
                    .collect(),
            )
        }

        fn set(&mut self, key: &'static str, value: impl AsRef<OsStr>) {
            unsafe { std::env::set_var(key, value) };
        }

        fn remove(&mut self, key: &'static str) {
            unsafe { std::env::remove_var(key) };
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe {
                for (key, previous) in self.0.drain(..) {
                    match previous {
                        Some(value) => std::env::set_var(key, value),
                        None => std::env::remove_var(key),
                    }
                }
            }
        }
    }

    #[test]
    #[serial]
    fn test_env_guard_restores_after_unwind() {
        const KEY: &str = "TOKSCALE_CLIENTS_ENV_GUARD_SELF_CHECK";
        let mut outer = EnvGuard::capture(&[KEY]);
        outer.set(KEY, "before");
        let result = std::panic::catch_unwind(|| {
            let mut inner = EnvGuard::capture(&[KEY]);
            inner.set(KEY, "during");
            panic!("exercise EnvGuard unwinding");
        });
        assert!(result.is_err());
        assert_eq!(std::env::var_os(KEY), Some("before".into()));
    }

    #[test]
    fn test_client_id_count() {
        assert_eq!(ClientId::COUNT, 31);
    }

    #[test]
    fn test_client_id_all_len_matches_count() {
        assert_eq!(ClientId::ALL.len(), ClientId::COUNT);
    }

    #[test]
    fn test_client_id_string_round_trip() {
        for client in ClientId::iter() {
            let id = client.as_str();
            assert_eq!(ClientId::from_str(id), Some(client));
        }
    }

    #[test]
    fn test_warp_client_registered_as_aggregate_cache_source() {
        let client = ClientId::from_str("warp").expect("warp client should be registered");
        assert_eq!(client.data().relative_path, "warp-cache");
        assert_eq!(client.data().pattern, "usage*.json");
        assert!(!client.data().parse_local);
        assert!(!client.data().submit_default);
    }

    #[test]
    fn test_path_root_home_resolves_to_home_dir() {
        let home = "/tmp/home";
        assert_eq!(PathRoot::Home.resolve(home), home);
    }

    #[test]
    #[serial]
    fn test_path_root_xdg_data_uses_env_var_when_set() {
        let mut _env = EnvGuard::capture(&["XDG_DATA_HOME"]);
        _env.set("XDG_DATA_HOME", "/tmp/xdg-data-home");

        let resolved = PathRoot::XdgData.resolve("/tmp/home");
        assert_eq!(resolved, "/tmp/xdg-data-home");
    }

    #[test]
    #[serial]
    fn test_path_root_xdg_data_falls_back_when_unset() {
        let mut _env = EnvGuard::capture(&["XDG_DATA_HOME"]);
        _env.remove("XDG_DATA_HOME");

        let resolved = PathRoot::XdgData.resolve("/tmp/home");
        assert_eq!(resolved, "/tmp/home/.local/share");
    }

    #[test]
    #[serial]
    fn test_path_root_xdg_data_ignores_env_when_disabled() {
        let mut _env = EnvGuard::capture(&["XDG_DATA_HOME"]);
        _env.set("XDG_DATA_HOME", "/tmp/xdg-data-home");

        let resolved = PathRoot::XdgData.resolve_with_env_strategy("/tmp/home", false);
        assert_eq!(resolved, "/tmp/home/.local/share");
    }

    #[test]
    #[serial]
    fn test_path_root_config_uses_override_when_set() {
        let mut _env = EnvGuard::capture(&["TOKSCALE_CONFIG_DIR", "XDG_CONFIG_HOME"]);
        _env.set("TOKSCALE_CONFIG_DIR", "/tmp/custom-config-root");
        _env.set("XDG_CONFIG_HOME", "/tmp/xdg-config-home");

        let resolved = PathRoot::Config.resolve("/tmp/home");
        assert_eq!(resolved, "/tmp/custom-config-root");
    }

    #[test]
    #[serial]
    #[cfg(target_os = "linux")]
    fn test_path_root_config_uses_xdg_config_home_when_override_unset() {
        let mut _env = EnvGuard::capture(&["TOKSCALE_CONFIG_DIR", "XDG_CONFIG_HOME"]);
        _env.remove("TOKSCALE_CONFIG_DIR");
        _env.set("XDG_CONFIG_HOME", "/tmp/xdg-config-home");

        let resolved = PathRoot::Config.resolve("/tmp/home");
        assert_eq!(resolved, "/tmp/xdg-config-home/tokscale");
    }

    #[test]
    #[serial]
    #[cfg(target_os = "windows")]
    fn test_path_root_config_uses_dirs_config_dir_on_windows() {
        // Windows must resolve PathRoot::Config to the same root that
        // paths::get_config_dir() and get_antigravity_cache_dir() use,
        // i.e. dirs::config_dir() (= %APPDATA%\tokscale). Hardcoding
        // {home}/.config/tokscale would diverge from the writer side
        // and silently hide synced Antigravity data from reports.
        let mut _env = EnvGuard::capture(&["TOKSCALE_CONFIG_DIR"]);
        _env.remove("TOKSCALE_CONFIG_DIR");

        let resolved = PathRoot::Config.resolve("C:\\fake-home");
        let expected = dirs::config_dir()
            .expect("Windows always exposes dirs::config_dir")
            .join("tokscale")
            .to_string_lossy()
            .into_owned();
        assert_eq!(
            resolved, expected,
            "PathRoot::Config on Windows must match dirs::config_dir().join('tokscale') so the scanner agrees with the writer"
        );
    }

    #[test]
    #[serial]
    fn test_path_root_config_ignores_env_when_disabled() {
        let mut _env = EnvGuard::capture(&["TOKSCALE_CONFIG_DIR", "XDG_CONFIG_HOME"]);
        _env.set("TOKSCALE_CONFIG_DIR", "/tmp/custom-config-root");
        _env.set("XDG_CONFIG_HOME", "/tmp/xdg-config-home");

        let resolved = PathRoot::Config.resolve_with_env_strategy("/tmp/home", false);
        assert_eq!(resolved, "/tmp/home/.config/tokscale");
    }

    #[test]
    #[serial]
    fn test_path_root_env_var_uses_env_when_set() {
        let var = "TOKSCALE_TEST_PATH_ROOT";
        let mut _env = EnvGuard::capture(&[var]);
        _env.set(var, "/tmp/custom-root");

        let root = PathRoot::EnvVar {
            var,
            fallback_relative: ".fallback",
        };
        let resolved = root.resolve("/tmp/home");
        assert_eq!(resolved, "/tmp/custom-root");
    }

    #[test]
    #[serial]
    fn test_path_root_env_var_falls_back_when_unset() {
        let var = "TOKSCALE_TEST_PATH_ROOT";
        let mut _env = EnvGuard::capture(&[var]);
        _env.remove(var);

        let root = PathRoot::EnvVar {
            var,
            fallback_relative: ".fallback",
        };
        let resolved = root.resolve("/tmp/home");
        assert_eq!(resolved, "/tmp/home/.fallback");
    }

    #[test]
    #[serial]
    fn test_path_root_env_var_ignores_env_when_disabled() {
        let var = "TOKSCALE_TEST_PATH_ROOT";
        let mut _env = EnvGuard::capture(&[var]);
        _env.set(var, "/tmp/custom-root");

        let root = PathRoot::EnvVar {
            var,
            fallback_relative: ".fallback",
        };
        let resolved = root.resolve_with_env_strategy("/tmp/home", false);
        assert_eq!(resolved, "/tmp/home/.fallback");
    }

    #[test]
    fn test_client_def_resolve_path_combines_root_and_relative() {
        let client = ClientDef {
            id: "test",
            root: PathRoot::Home,
            relative_path: ".test/sessions",
            pattern: "*.jsonl",
            headless: false,
            parse_local: true,
            submit_default: true,
        };

        assert_eq!(client.resolve_path("/tmp/home"), "/tmp/home/.test/sessions");
    }

    #[test]
    fn test_client_id_iter_yields_all_in_order() {
        let all: Vec<ClientId> = ClientId::iter().collect();
        assert_eq!(all, ClientId::ALL);
    }

    #[test]
    fn test_client_counts_get_set_add_work() {
        let mut counts = ClientCounts::new();

        assert_eq!(counts.get(ClientId::Claude), 0);
        counts.set(ClientId::Claude, 3);
        assert_eq!(counts.get(ClientId::Claude), 3);
        counts.add(ClientId::Claude, 2);
        assert_eq!(counts.get(ClientId::Claude), 5);
    }

    #[test]
    fn test_codex_root_uses_codex_home_env_var() {
        assert_eq!(
            ClientId::Codex.data().root,
            PathRoot::EnvVar {
                var: "CODEX_HOME",
                fallback_relative: ".codex",
            }
        );
    }

    #[test]
    fn test_cursor_parse_local_is_false() {
        assert!(!ClientId::Cursor.data().parse_local);
    }

    #[test]
    fn test_crush_submit_default_is_false() {
        assert!(!ClientId::Crush.submit_default());
    }

    #[test]
    fn test_hermes_root_uses_hermes_home_env_var() {
        assert_eq!(
            ClientId::Hermes.data().root,
            PathRoot::EnvVar {
                var: "HERMES_HOME",
                fallback_relative: ".hermes",
            }
        );
        assert_eq!(ClientId::Hermes.data().relative_path, "state.db");
    }

    #[test]
    fn test_codebuff_root_uses_codebuff_data_dir_env_var() {
        assert_eq!(
            ClientId::Codebuff.data().root,
            PathRoot::EnvVar {
                var: "CODEBUFF_DATA_DIR",
                fallback_relative: ".config/manicode",
            }
        );
        assert_eq!(ClientId::Codebuff.data().pattern, "chat-messages.json");
    }

    #[test]
    fn test_antigravity_parse_local_is_true() {
        assert!(ClientId::Antigravity.data().parse_local);
    }

    #[test]
    fn test_antigravity_submit_default_is_false() {
        assert!(!ClientId::Antigravity.submit_default());
    }

    #[test]
    #[serial]
    fn test_zed_data_dir_path() {
        let mut _env = EnvGuard::capture(&["XDG_DATA_HOME"]);
        _env.remove("XDG_DATA_HOME");

        assert_eq!(
            ClientId::Zed.data().resolve_path("/tmp/home"),
            "/tmp/home/.local/share/zed/threads/threads.db"
        );
    }

    #[test]
    fn test_zed_submit_default_is_true() {
        assert!(ClientId::Zed.submit_default());
    }

    #[test]
    fn test_kiro_data_dir_path() {
        assert_eq!(
            ClientId::Kiro.data().resolve_path("/tmp/home"),
            "/tmp/home/.kiro/sessions/cli"
        );
        assert_eq!(ClientId::Kiro.data().pattern, "*.json");
        assert!(ClientId::Kiro.parse_local());
        assert!(ClientId::Kiro.submit_default());
        assert!(!ClientId::Kiro.supports_headless());
    }

    #[test]
    fn test_grok_client_registered_as_local_session_source() {
        let client = ClientId::from_str("grok").expect("grok client should be registered");
        assert_eq!(client.data().relative_path, "sessions");
        assert_eq!(client.data().pattern, "updates.jsonl");
        assert!(client.data().parse_local);
        assert!(client.data().submit_default);
        assert_eq!(
            client
                .data()
                .resolve_path_with_env_strategy("/tmp/home", false),
            "/tmp/home/.grok/sessions"
        );
    }
}
