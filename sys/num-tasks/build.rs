// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::env;
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out = &PathBuf::from(env::var_os("OUT_DIR").unwrap());

    println!("cargo:rerun-if-env-changed=HUBRIS_TASKS");
    let mut task_enum = vec![];
    if let Ok(task_names) = env::var("HUBRIS_TASKS") {
        println!("HUBRIS_TASKS = {}", task_names);
        for (i, name) in task_names.split(",").enumerate() {
            task_enum.push(format!("    {} = {},", name, i));
        }
    } else {
        panic!("can't build this crate outside of the build system.");
    }
    let task_count = task_enum.len();

    let mut task_file = File::create(out.join("tasks.rs")).unwrap();

    if env::var_os("CARGO_FEATURE_TASK_ENUM").is_some() {
        writeln!(task_file, "#[allow(non_camel_case_types)]").unwrap();
        writeln!(task_file, "pub enum Task {{").unwrap();
        for line in task_enum {
            writeln!(task_file, "{}", line).unwrap();
        }
        writeln!(task_file, "}}").unwrap();
    }
    writeln!(task_file, "pub const NUM_TASKS: usize = {};", task_count)
        .unwrap();

    Ok(())
}
