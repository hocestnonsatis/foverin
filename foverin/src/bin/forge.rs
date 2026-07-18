//! The Forge — synthesize process windows, train the micro-network, export weights.
//!
//! ```text
//! cargo run --release --bin forge
//! ```

use anyhow::{Context as _, Result};
use candle_core::{D, DType, Device, Tensor};
use candle_nn::{Optimizer, VarBuilder, VarMap, loss, ops};
use foverin::brain::{
    Classifier, DEFAULT_WEIGHTS_PATH, VOCAB, Workload, WorkloadNet, encode_names,
    resolve_weights_path,
};
use rand::{Rng, SeedableRng, rngs::StdRng, seq::SliceRandom};

const EPOCHS: usize = 200;
const LEARNING_RATE: f64 = 0.05;
const TRAIN_SAMPLES: usize = 2_048;
const TEST_SAMPLES: usize = 512;

fn main() -> Result<()> {
    println!(
        "[FORGE] Ignition — synthesizing {TRAIN_SAMPLES} train + {TEST_SAMPLES} test windows…"
    );

    let mut rng = StdRng::seed_from_u64(0x00c0_ffee_71c0);
    let (train_x, train_y) = synthesize_batch(TRAIN_SAMPLES, &mut rng);
    let (test_x, test_y) = synthesize_batch(TEST_SAMPLES, &mut rng);

    let device = Device::Cpu;
    let train_images = Tensor::from_vec(train_x, (TRAIN_SAMPLES, VOCAB.len()), &device)?;
    let train_labels = Tensor::from_vec(train_y, TRAIN_SAMPLES, &device)?;
    let test_images = Tensor::from_vec(test_x, (TEST_SAMPLES, VOCAB.len()), &device)?;
    let test_labels = Tensor::from_vec(test_y, TEST_SAMPLES, &device)?;

    let varmap = VarMap::new();
    let vs = VarBuilder::from_varmap(&varmap, DType::F32, &device);
    let model = WorkloadNet::new(vs).context("init WorkloadNet")?;
    let mut sgd = candle_nn::SGD::new(varmap.all_vars(), LEARNING_RATE)?;

    println!(
        "[FORGE] Architecture: {} → 64 → 32 → 4  |  lr={LEARNING_RATE}  epochs={EPOCHS}",
        VOCAB.len()
    );

    for epoch in 1..=EPOCHS {
        let logits = model.forward(&train_images)?;
        let log_sm = ops::log_softmax(&logits, D::Minus1)?;
        let loss_val = loss::nll(&log_sm, &train_labels)?;
        sgd.backward_step(&loss_val)?;

        if epoch == 1 || epoch % 25 == 0 || epoch == EPOCHS {
            let test_logits = model.forward(&test_images)?;
            let sum_ok = test_logits
                .argmax(D::Minus1)?
                .eq(&test_labels)?
                .to_dtype(DType::F32)?
                .sum_all()?
                .to_scalar::<f32>()?;
            let acc = 100.0 * sum_ok / TEST_SAMPLES as f32;
            println!(
                "[FORGE] epoch {epoch:3}  loss={:.5}  test_acc={acc:5.1}%",
                loss_val.to_scalar::<f32>()?
            );
        }
    }

    let out = resolve_weights_path();
    // Prefer writing the canonical filename in CWD when no env override / existing file.
    let out = if std::env::var_os("FOVERIN_WEIGHTS").is_some() {
        out
    } else {
        std::path::PathBuf::from(DEFAULT_WEIGHTS_PATH)
    };
    varmap
        .save(&out)
        .with_context(|| format!("save weights to {}", out.display()))?;

    let meta = std::fs::metadata(&out)?;
    println!("[FORGE] Exported {} ({} bytes)", out.display(), meta.len());

    // Sanity: reload and classify a few hand-crafted windows.
    let clf = Classifier::load(&out)?;
    let probes: &[(&[&str], Workload)] = &[
        (&["cargo", "rustc"], Workload::Compiling),
        (&["steam", "cs2"], Workload::Gaming),
        (&["firefox", "spotify"], Workload::Browsing),
        (&["bash", "sleep"], Workload::Idle),
        (&[], Workload::Idle),
    ];
    println!("[FORGE] Crucible probes:");
    for (names, expected) in probes {
        let v = encode_names(*names);
        let start = std::time::Instant::now();
        let (got, confidence) = clf.classify_vector(&v)?;
        let us = start.elapsed().as_micros();
        let mark = if got == *expected { "OK" } else { "FAIL" };
        println!(
            "  [{mark}] {:?} → {} ({confidence:.1}%) (expected {})  [{us} µs]",
            names,
            got.as_str(),
            expected.as_str()
        );
        anyhow::ensure!(
            got == *expected,
            "probe failed: {:?} predicted {} expected {}",
            names,
            got.as_str(),
            expected.as_str()
        );
    }

    println!("[FORGE] Quench complete. Weights ready for Foverin.");
    Ok(())
}

fn synthesize_batch(n: usize, rng: &mut StdRng) -> (Vec<f32>, Vec<u32>) {
    let mut xs = Vec::with_capacity(n * VOCAB.len());
    let mut ys = Vec::with_capacity(n);
    for _ in 0..n {
        let (names, label) = sample_window(rng);
        xs.extend(encode_names(&names));
        ys.push(label.index() as u32);
    }
    (xs, ys)
}

/// Draw a labelled synthetic 5-second process window.
fn sample_window(rng: &mut StdRng) -> (Vec<&'static str>, Workload) {
    // Class prior — slightly favour IDLE so the daemon is conservative when quiet.
    let class = match rng.gen_range(0..10u8) {
        0..=2 => Workload::Compiling,
        3..=5 => Workload::Gaming,
        6..=7 => Workload::Browsing,
        _ => Workload::Idle,
    };

    let mut names: Vec<&'static str> = match class {
        Workload::Compiling => pick_subset(
            rng,
            &[
                "rustc", "cargo", "cc", "gcc", "clang", "make", "ninja", "ld", "lld", "cmake",
            ],
            2,
            5,
        ),
        Workload::Gaming => pick_subset(
            rng,
            &[
                "steam",
                "steamwebhelper",
                "csgo",
                "cs2",
                "dota2",
                "proton",
                "wine",
                "wine64",
                "gamesoverlayui",
            ],
            2,
            4,
        ),
        Workload::Browsing => pick_subset(
            rng,
            &[
                "firefox", "chrome", "chromium", "brave", "spotify", "code", "electron", "slack",
                "discord",
            ],
            1,
            3,
        ),
        Workload::Idle => {
            if rng.gen_bool(0.35) {
                Vec::new()
            } else {
                pick_subset(
                    rng,
                    &[
                        "bash", "zsh", "sh", "fish", "systemd", "sshd", "sleep", "login", "sudo",
                    ],
                    1,
                    3,
                )
            }
        }
    };

    // Occasional shell noise on non-IDLE windows (realistic exec chatter).
    if class != Workload::Idle && rng.gen_bool(0.25) {
        names.extend(pick_subset(rng, &["bash", "zsh", "sh"], 1, 1));
    }

    names.shuffle(rng);
    (names, class)
}

fn pick_subset(rng: &mut StdRng, pool: &[&'static str], lo: usize, hi: usize) -> Vec<&'static str> {
    let k = rng.gen_range(lo..=hi).min(pool.len());
    let mut pool = pool.to_vec();
    pool.shuffle(rng);
    pool.truncate(k);
    pool
}
