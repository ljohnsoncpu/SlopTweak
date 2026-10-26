//! A stand-in for the instance side in mock mode, so the Train tab can be
//! exercised end to end without renting anything. Time-driven: setup, then
//! training, then test images, then done.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Instant;

use async_trait::async_trait;

use super::run::{Progress, RemoteTrain};

const SETUP_SECS: f64 = 6.0;
const STEPS_PER_SEC: f64 = 40.0;
const IMAGES_PER_SEC: f64 = 3.0;

pub struct SimTrain {
    results_dir: PathBuf,
    total_steps: u32,
    eval_images: u32,
    began: Mutex<Option<Instant>>,
}

impl SimTrain {
    pub fn new(results_dir: PathBuf, total_steps: u32, eval_images: usize) -> Self {
        Self {
            results_dir,
            total_steps,
            eval_images: eval_images as u32,
            began: Mutex::new(None),
        }
    }

    fn at(&self, secs: f64) -> Progress {
        if secs < SETUP_SECS {
            return Progress::Starting;
        }
        let train = secs - SETUP_SECS;
        let train_secs = f64::from(self.total_steps) / STEPS_PER_SEC;
        if train < train_secs {
            return Progress::Training {
                step: (train * STEPS_PER_SEC) as u32,
                total: self.total_steps,
            };
        }
        let eval = train - train_secs;
        if eval * IMAGES_PER_SEC < f64::from(self.eval_images) {
            return Progress::Evaluating {
                done: (eval * IMAGES_PER_SEC) as u32,
                total: self.eval_images,
            };
        }
        Progress::Done
    }
}

#[async_trait]
impl RemoteTrain for SimTrain {
    async fn begin(&self, _: u64) -> Result<(), String> {
        self.began.lock().unwrap().get_or_insert_with(Instant::now);
        Ok(())
    }

    async fn progress(&self, _: u64) -> Result<Progress, String> {
        let began = *self.began.lock().unwrap();
        Ok(began.map_or(Progress::Starting, |t| self.at(t.elapsed().as_secs_f64())))
    }

    async fn sync(&self, _: u64) -> Result<bool, String> {
        std::fs::create_dir_all(self.results_dir.join("checkpoints")).map_err(|e| e.to_string())?;
        std::fs::write(
            self.results_dir.join("checkpoints").join("simulated.txt"),
            "Mock mode: no GPU was rented and no LoRA was trained.\n",
        )
        .map_err(|e| e.to_string())?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_runs_through_every_stage() {
        let s = SimTrain::new(PathBuf::new(), 400, 52);
        assert_eq!(s.at(0.0), Progress::Starting);
        assert_eq!(
            s.at(7.0),
            Progress::Training {
                step: 40,
                total: 400
            }
        );
        // 400 steps take 10 s after the 6 s setup.
        assert!(matches!(
            s.at(17.0),
            Progress::Evaluating { done: 3, total: 52 }
        ));
        assert_eq!(s.at(6.0 + 10.0 + 52.0 / 3.0 + 1.0), Progress::Done);
    }

    #[tokio::test]
    async fn sync_writes_a_visible_marker() {
        let d = tempfile::tempdir().unwrap();
        let s = SimTrain::new(d.path().join("results"), 4, 4);
        assert_eq!(s.progress(1).await.unwrap(), Progress::Starting);
        assert!(s.sync(1).await.unwrap());
        assert!(d.path().join("results/checkpoints/simulated.txt").is_file());
    }
}
