//! `onset make-schedule` from the command line.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("make_schedule").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn make_schedule(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_onset")).arg("make-schedule").args(args).output().unwrap()
}

#[test]
fn reproduces_the_python_schedule_and_validates() {
    // tests/data/make_schedule_run1.tsv is what the Python script drew with
    // seed 1, run 1 from 80 images. Empty files stand in for them: only
    // their names and order matter here.
    let dir = scratch("example");
    let images = dir.join("images");
    std::fs::create_dir_all(&images).unwrap();
    for i in 0..80 {
        std::fs::write(images.join(format!("img{i:03}.png")), b"").unwrap();
    }
    let exp = dir.join("experiment.toml");
    std::fs::copy("examples/nsd/experiment.toml", &exp).unwrap();
    let out = dir.join("schedule.tsv");
    let o = make_schedule(&[exp.to_str().unwrap(), "--run", "1", "--images", images.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(String::from_utf8_lossy(&o.stdout).contains("63 image trials (22 old), 5 blank slots, run 1"));
    assert_eq!(
        std::fs::read_to_string(&out).unwrap(),
        std::fs::read_to_string("tests/data/make_schedule_run1.tsv").unwrap()
    );

    // The example ships without a schedule, so CI checks it here, with this one.
    let o = Command::new(env!("CARGO_BIN_EXE_onset"))
        .arg("validate")
        .arg(&exp)
        .args(["--rig", "examples/rigs/dev.toml"])
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
}

#[test]
fn history_carries_old_and_new_across_runs() {
    let dir = scratch("history");
    let images = dir.join("images");
    std::fs::create_dir_all(&images).unwrap();
    for i in 0..150 {
        std::fs::write(images.join(format!("shared{i:04}_nsd{:05}.png", i * 7 + 1)), b"").unwrap();
    }
    std::fs::write(images.join("._shared0000_nsd00001.png"), b"").unwrap(); // macOS metadata file: skipped
    let history = dir.join("sub-01").join("seen.txt");
    let mut all_new = Vec::new();
    for run in ["1", "2"] {
        let out = dir.join(format!("run{run}.tsv"));
        let o = make_schedule(&[
            "examples/nsd/experiment.toml",
            "--run",
            run,
            "--images",
            images.to_str().unwrap(),
            "--history",
            history.to_str().unwrap(),
            "--out",
            out.to_str().unwrap(),
        ]);
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        let text = std::fs::read_to_string(&out).unwrap();
        let mut lines = text.lines();
        assert_eq!(lines.next(), Some("onset\tstim_file\tmemory\tanswer\timage\tnsd_id"));
        let rows: Vec<Vec<&str>> = lines.map(|l| l.split('\t').collect()).collect();
        assert_eq!(rows.len(), if run == "1" { 63 } else { 62 });
        for row in &rows {
            let seen_before = all_new.contains(&row[4].to_string());
            assert_eq!(row[2], if seen_before { "old" } else { "new" }, "{row:?}");
            assert_eq!(row[3], if seen_before { "2" } else { "1" });
            assert!(Path::new(row[1]).is_absolute() && row[1].ends_with(&format!("{}.png", row[4])));
            let id: u32 = row[5].parse().unwrap();
            assert_eq!(format!("{id:05}"), row[4][row[4].len() - 5..]);
            if !seen_before {
                all_new.push(row[4].to_string());
            }
        }
        let listed: Vec<String> =
            std::fs::read_to_string(&history).unwrap().lines().map(str::to_string).collect();
        assert_eq!(listed, all_new, "the history lists every image shown, once, in order");
    }
}

#[test]
fn explains_what_is_missing() {
    let o = make_schedule(&["examples/flanker/experiment.toml"]);
    assert!(!o.status.success());
    assert!(String::from_utf8_lossy(&o.stderr).contains("no [schedule.generate] table"));

    let dir = scratch("bad");
    let exp = dir.join("experiment.toml");
    let text = std::fs::read_to_string("examples/nsd/experiment.toml")
        .unwrap()
        .replace("stretch = [9, 14]", "stretch = [12, 14]");
    std::fs::write(&exp, text).unwrap();
    let o = make_schedule(&[exp.to_str().unwrap()]);
    assert!(
        String::from_utf8_lossy(&o.stderr).contains("it needs min <= 10 and max >= 11"),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
}
