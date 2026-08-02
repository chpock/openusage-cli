use std::path::PathBuf;
use std::process::Command;

const CHILD_CASE_ENV: &str = "OPENUSAGE_HERMETIC_TEST_CASE";
const ROOT_ENV: &str = "OPENUSAGE_HERMETIC_TEST_ROOT";

#[derive(Clone, Debug)]
pub struct HermeticPaths {
    pub root: PathBuf,
    pub app_data: PathBuf,
    pub overrides: PathBuf,
}

impl HermeticPaths {
    fn from_root(root: PathBuf) -> Self {
        Self {
            app_data: root.join("app-data"),
            overrides: root.join("overrides"),
            root,
        }
    }
}

/// Run one filesystem integration test in a fresh child process whose complete
/// home/XDG/tmp environment points inside a temporary root. The parent never
/// reads, changes, or restores user environment variables.
///
/// Returns the child's paths when called from that child. In the parent it
/// waits for the child and returns `None`, so the test should return early.
pub fn enter(case_name: &str) -> Option<HermeticPaths> {
    if std::env::var(CHILD_CASE_ENV).ok().as_deref() == Some(case_name) {
        let root = PathBuf::from(
            std::env::var(ROOT_ENV).expect("hermetic child must receive its temporary root"),
        );
        assert_eq!(
            dirs::home_dir().as_deref(),
            Some(root.as_path()),
            "hermetic child HOME must resolve to its temporary root"
        );
        for (name, expected) in [
            ("XDG_CONFIG_HOME", root.join(".config")),
            ("XDG_DATA_HOME", root.join(".local/share")),
            ("XDG_CACHE_HOME", root.join(".cache")),
            ("XDG_STATE_HOME", root.join(".local/state")),
            ("XDG_RUNTIME_DIR", root.join(".run")),
            ("TMPDIR", root.join("tmp")),
            ("TEMPDIR", root.join("tmp")),
            ("CODEX_HOME", root.join(".codex")),
        ] {
            assert_eq!(
                std::env::var_os(name).as_deref(),
                Some(expected.as_os_str()),
                "hermetic child {name} must point inside its temporary root"
            );
            assert!(
                expected.is_dir(),
                "hermetic child {name} directory must exist"
            );
        }
        return Some(HermeticPaths::from_root(root));
    }

    let temp = tempfile::tempdir().expect("create hermetic test root");
    let paths = HermeticPaths::from_root(temp.path().to_path_buf());
    create_layout(&paths);

    let status = Command::new(std::env::current_exe().expect("locate test binary"))
        .arg("--exact")
        .arg(case_name)
        .arg("--nocapture")
        .env_clear()
        .env(CHILD_CASE_ENV, case_name)
        .env(ROOT_ENV, &paths.root)
        .env("HOME", &paths.root)
        .env("XDG_CONFIG_HOME", paths.root.join(".config"))
        .env("XDG_DATA_HOME", paths.root.join(".local/share"))
        .env("XDG_CACHE_HOME", paths.root.join(".cache"))
        .env("XDG_STATE_HOME", paths.root.join(".local/state"))
        .env("XDG_RUNTIME_DIR", paths.root.join(".run"))
        .env("TMPDIR", paths.root.join("tmp"))
        .env("TEMPDIR", paths.root.join("tmp"))
        .env("CODEX_HOME", paths.root.join(".codex"))
        .status()
        .expect("start hermetic test child");

    assert!(status.success(), "hermetic child test {case_name} failed");
    None
}

fn create_layout(paths: &HermeticPaths) {
    for path in [
        paths.root.clone(),
        paths.root.join(".config"),
        paths.root.join(".local/share"),
        paths.root.join(".local/state"),
        paths.root.join(".cache"),
        paths.root.join(".run"),
        paths.root.join("tmp"),
        paths.root.join(".codex"),
        paths.app_data.clone(),
        paths.overrides.clone(),
    ] {
        std::fs::create_dir_all(path).expect("create hermetic test directory");
    }
}
