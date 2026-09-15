use std::{env, fs, path::PathBuf};

const DATABASE_TESTS: usize = 1_600;
const CPU_TESTS: usize = 4_400;

fn main() {
    let output = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("generated_tests.rs");
    let mut tests = String::new();
    for index in 0..DATABASE_TESTS {
        let kind = if index.is_multiple_of(12) {
            "isolated"
        } else {
            "reusable"
        };
        tests.push_str(&format!(
            "#[test]\nfn database_{kind}_{index:04}() {{ crate::run_database_test({index}); }}\n"
        ));
    }
    for index in 0..CPU_TESTS {
        tests.push_str(&format!(
            "#[test]\nfn cpu_{index:04}() {{ crate::run_cpu_test({index}); }}\n"
        ));
    }
    fs::write(output, tests).unwrap();
    println!("cargo:rerun-if-changed=build.rs");
}
