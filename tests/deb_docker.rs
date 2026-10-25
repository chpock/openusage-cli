#[path = "support/hermetic.rs"]
mod hermetic;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn fixture(root: &Path, exit_code: i32) -> (PathBuf, PathBuf) {
    let project = root.join("project with spaces");
    fs::create_dir(&project).unwrap();
    fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("Makefile"),
        project.join("Makefile"),
    )
    .unwrap();
    let docker = root.join("docker with spaces");
    fs::write(
        &docker,
        format!(
            "#!/bin/sh\nprintf '%s\\n' invocation >> \"$0.calls\"\nprintf '%s\\n' \"$@\" > \"$0.args\"\nprintf '%s\\n' \"${{DOCKER_BUILDKIT:-}}\" > \"$0.buildkit\"\nexit {exit_code}\n"
        ),
    )
    .unwrap();
    fs::set_permissions(&docker, fs::Permissions::from_mode(0o755)).unwrap();
    (project, docker)
}

fn run(project: &Path, docker: &Path) -> Output {
    Command::new("make")
        .arg("--no-print-directory")
        .arg("-C")
        .arg(project)
        .arg(format!("DOCKER={}", docker.display()))
        .arg("deb-docker")
        .env("PATH", "/usr/bin:/bin")
        .output()
        .unwrap()
}

fn option<'a>(arguments: &'a [String], name: &str) -> &'a str {
    let index = arguments.iter().position(|arg| arg == name).unwrap();
    &arguments[index + 1]
}

#[test]
fn builds_from_project_with_separate_output_and_does_not_skip_existing_target_file() {
    let Some(paths) = hermetic::enter(
        "builds_from_project_with_separate_output_and_does_not_skip_existing_target_file",
    ) else {
        return;
    };
    let (project, docker) = fixture(&paths.root, 0);
    fs::write(project.join("deb-docker"), b"not a build result").unwrap();
    for _ in 0..2 {
        let output = run(&project, &docker);
        assert!(
            output.status.success(),
            "make failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let arguments: Vec<String> = fs::read_to_string(docker.with_extension("args"))
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(arguments[0], "build");
    assert_eq!(arguments.last().unwrap(), project.to_str().unwrap());
    assert_eq!(
        option(&arguments, "--file"),
        project
            .join("dist/docker/debian.Dockerfile")
            .to_str()
            .unwrap()
    );
    assert_eq!(
        option(&arguments, "--output"),
        format!("type=local,dest={}/target/debian-docker", project.display())
    );
    assert_eq!(
        fs::read_to_string(docker.with_extension("buildkit")).unwrap(),
        "1\n"
    );
    assert_eq!(
        fs::read_to_string(docker.with_extension("calls")).unwrap(),
        "invocation\ninvocation\n"
    );
    assert_eq!(
        fs::read(project.join("deb-docker")).unwrap(),
        b"not a build result"
    );
}

#[test]
fn reports_docker_failure_instead_of_claiming_a_package_was_built() {
    let Some(paths) =
        hermetic::enter("reports_docker_failure_instead_of_claiming_a_package_was_built")
    else {
        return;
    };
    let (project, docker) = fixture(&paths.root, 23);
    let output = run(&project, &docker);
    assert!(!output.status.success());
    assert_eq!(
        fs::read_to_string(docker.with_extension("calls")).unwrap(),
        "invocation\n"
    );
    assert!(
        !project
            .join("target/debian-docker/openusage-cli.deb")
            .exists()
    );
}

#[test]
fn dependency_check_preserves_metadata_and_rejects_empty_or_failed_lookup() {
    let Some(paths) =
        hermetic::enter("dependency_check_preserves_metadata_and_rejects_empty_or_failed_lookup")
    else {
        return;
    };
    let tools = paths.root.join("tools");
    fs::create_dir(&tools).unwrap();
    let dpkg = tools.join("dpkg-deb");
    fs::write(
        &dpkg,
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$0.args\"\nprintf '%s' \"$FAKE_DEPENDS\"\nexit \"$FAKE_CODE\"\n",
    )
    .unwrap();
    fs::set_permissions(&dpkg, fs::Permissions::from_mode(0o755)).unwrap();
    let package = paths.root.join("package with spaces.deb");
    fs::write(&package, b"controlled package reader input").unwrap();
    for (depends, code, succeeds) in [
        ("libc6 (>= 2.38), libgcc-s1 (>= 4.2)", "0", true),
        ("", "0", false),
        (" \t\n", "0", false),
        ("libc6", "23", false),
    ] {
        let output = Command::new("/bin/sh")
            .arg(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("dist/scripts/check-deb-dependencies.sh"),
            )
            .arg(&package)
            .env("PATH", &tools)
            .env("FAKE_DEPENDS", depends)
            .env("FAKE_CODE", code)
            .output()
            .unwrap();
        assert_eq!(
            output.status.success(),
            succeeds,
            "metadata={depends:?}, code={code}, stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            fs::read_to_string(dpkg.with_extension("args")).unwrap(),
            format!("--field\n{}\nDepends\n", package.display())
        );
        if succeeds {
            assert_eq!(
                String::from_utf8(output.stdout).unwrap(),
                format!("{depends}\n")
            );
        }
    }
}
