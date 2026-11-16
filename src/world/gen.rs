//! The generation pool around `arkh-gen`.
//!
//! Worlds are made on a few plain threads, never on the async runtime, so
//! relay traffic never waits on a generation. A running generation cannot
//! be stopped: the time limit only ends the wait for it (the caller gets
//! `gen-failed`), and the thread finishes the work and drops the result.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;
use tokio::sync::oneshot;

pub use arkh_gen::{Config as GenConfig, World};

/// The generator crate's name, as stored in a world.
pub const CRATE: &str = "arkh-gen";

/// The linked generator's version.
pub fn version() -> &'static str {
    arkh_gen::gen::GENERATOR_VERSION
}

/// The hash of the logic data and game facts compiled into the generator.
pub fn data_hash() -> String {
    format!("{:016x}", arkh_gen::data::data_hash())
}

/// A world's hash, as `arkh-gen hash` prints it: FNV-1a over the world's
/// JSON.
pub fn world_hash(w: &World) -> String {
    let bytes = serde_json::to_vec(w).expect("a world serializes");
    format!("{:016x}", arkh_gen::rng::fnv1a(&[&bytes]))
}

static MODEL: OnceLock<Result<arkh_gen::model::Model, String>> = OnceLock::new();

/// The compiled logic, built once on first use (on a pool thread).
fn model() -> Result<&'static arkh_gen::model::Model, String> {
    MODEL
        .get_or_init(arkh_gen::model)
        .as_ref()
        .map_err(|e| format!("the generator's logic did not build: {e}"))
}

/// Make a world now, on this thread.
pub fn make(cfg: &GenConfig) -> Result<(Arc<World>, String), String> {
    let m = model()?;
    let w = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| arkh_gen::generate(m, cfg)))
        .map_err(|_| "the generator stopped on an internal error".to_string())??;
    let h = world_hash(&w);
    Ok((Arc::new(w), h))
}

/// What a generation gives: the world and its hash, or why it failed.
pub type Made = Result<(Arc<World>, String), String>;

/// Told the job's place in the queue (1 is next) when it has to wait.
pub type OnWait = Box<dyn Fn(usize) + Send>;

/// A queued generation: `started` fires when a thread takes it, `done`
/// with the result.
#[derive(Debug)]
pub struct Ticket {
    pub started: oneshot::Receiver<()>,
    pub done: oneshot::Receiver<Made>,
}

impl Ticket {
    /// Wait for the result: as long as the job waits in the queue, then at
    /// most `limit` once a thread works on it.
    pub async fn wait(self, limit: Duration) -> Made {
        let _ = self.started.await;
        match tokio::time::timeout(limit, self.done).await {
            Ok(Ok(m)) => m,
            Ok(Err(_)) => Err("the generation thread stopped".into()),
            Err(_) => Err(format!("over the time limit of {} s", limit.as_secs_f64())),
        }
    }
}

struct Job {
    cfg: GenConfig,
    started: oneshot::Sender<()>,
    reply: oneshot::Sender<Made>,
    on_wait: Option<OnWait>,
    /// The last place sent, 0 when none was.
    told: usize,
}

struct Inner {
    queue: VecDeque<Job>,
    idle: usize,
    stop: bool,
}

/// Why a job was not queued.
#[derive(Debug, PartialEq, Eq)]
pub struct Full {
    pub queued: usize,
}

/// The pool.
pub struct Pool {
    inner: Mutex<Inner>,
    cv: Condvar,
    queue_cap: usize,
    pub time_limit: Duration,
}

impl Pool {
    /// Start `workers` threads. `queue_cap` jobs may wait.
    pub fn start(workers: usize, queue_cap: usize, time_limit: Duration) -> Arc<Pool> {
        let pool = Arc::new(Pool {
            inner: Mutex::new(Inner {
                queue: VecDeque::new(),
                // threads not working on a job (they may still be starting)
                idle: workers.max(1),
                stop: false,
            }),
            cv: Condvar::new(),
            queue_cap,
            time_limit,
        });
        for i in 0..workers.max(1) {
            let p = pool.clone();
            std::thread::Builder::new()
                .name(format!("metaxy-gen-{i}"))
                .spawn(move || p.work())
                .expect("start a generation thread");
        }
        pool
    }

    /// Jobs waiting now.
    pub fn queued(&self) -> usize {
        self.inner.lock().unwrap().queue.len()
    }

    /// Queue a generation. `front` puts it first and past the queue limit
    /// (a stored world made again for a join). `on_wait` hears the job's
    /// place while it waits.
    pub fn submit(
        &self,
        cfg: GenConfig,
        front: bool,
        on_wait: Option<OnWait>,
    ) -> Result<Ticket, Full> {
        let (tx, rx) = oneshot::channel();
        let (stx, srx) = oneshot::channel();
        let mut g = self.inner.lock().unwrap();
        if !front && g.queue.len() >= self.queue_cap {
            return Err(Full {
                queued: g.queue.len(),
            });
        }
        let job = Job {
            cfg,
            started: stx,
            reply: tx,
            on_wait,
            told: 0,
        };
        if front {
            g.queue.push_front(job);
        } else {
            g.queue.push_back(job);
        }
        Self::tell_places(&mut g);
        drop(g);
        self.cv.notify_one();
        Ok(Ticket {
            started: srx,
            done: rx,
        })
    }

    /// Send each waiting job its place when it changed. Jobs that an idle
    /// thread will take at once are not told.
    fn tell_places(g: &mut Inner) {
        let idle = g.idle;
        for (i, j) in g.queue.iter_mut().enumerate() {
            if i < idle {
                continue;
            }
            let place = i + 1 - idle;
            if place != j.told {
                j.told = place;
                if let Some(f) = &j.on_wait {
                    f(place);
                }
            }
        }
    }

    /// Stop the threads once the queue is empty.
    pub fn stop(&self) {
        self.inner.lock().unwrap().stop = true;
        self.cv.notify_all();
    }

    fn work(&self) {
        loop {
            let job = {
                let mut g = self.inner.lock().unwrap();
                while g.queue.is_empty() && !g.stop {
                    g = self.cv.wait(g).unwrap();
                }
                match g.queue.pop_front() {
                    Some(j) => {
                        g.idle -= 1;
                        Self::tell_places(&mut g);
                        j
                    }
                    None => return,
                }
            };
            let _ = job.started.send(());
            let made = make(&job.cfg);
            let _ = job.reply.send(made);
            self.inner.lock().unwrap().idle += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// arkhipelagos' tests/configs/two-players.json and its golden hash
    /// (tests/golden.txt) at the pinned commit.
    const TWO: &str = r#"{
  "schema": 1,
  "seed": 4242,
  "options": {"goal": "ghetsis", "season_control": "vanilla"},
  "players": [
    {"name": "Ash", "version": "black"},
    {"name": "Misty", "version": "white", "options": {"modify_logic": ["require_flash"]}}
  ]
}"#;
    const TWO_HASH: &str = "1b98415ea2bc4583";

    #[test]
    fn golden_hash_of_the_pinned_generator() {
        let cfg: GenConfig = serde_json::from_str(TWO).unwrap();
        let (w, h) = make(&cfg).unwrap();
        assert_eq!(h, TWO_HASH);
        assert_eq!(w.players.len(), 2);
        assert_eq!(w.players[0].slot, 1);
    }

    #[tokio::test]
    async fn pool_makes_worlds_and_refuses_past_its_queue() {
        let pool = Pool::start(1, 1, Duration::from_secs(30));
        let cfg: GenConfig = serde_json::from_str(TWO).unwrap();
        let a = pool.submit(cfg.clone(), false, None).unwrap();
        let (w, h) = a.wait(Duration::from_secs(30)).await.unwrap();
        assert_eq!((h.as_str(), w.players.len()), (TWO_HASH, 2));
        let mut bad = cfg.clone();
        bad.schema = 9;
        let e = pool
            .submit(bad, false, None)
            .unwrap()
            .wait(Duration::from_secs(30))
            .await;
        assert!(e.unwrap_err().contains("schema"));
        // one thread busy, one job waiting: the next is refused, a job in
        // front is not
        let places = Arc::new(Mutex::new(Vec::new()));
        let p2 = places.clone();
        let mut sixteen = cfg.clone();
        sixteen.players = (0..8)
            .map(|i| arkh_gen::PlayerConfig {
                name: format!("P{i}"),
                version: "black".into(),
                options: Default::default(),
            })
            .collect();
        let r1 = pool.submit(sixteen, false, None).unwrap();
        // wait until the thread took it
        while pool.queued() > 0 {
            std::thread::sleep(Duration::from_millis(5));
        }
        let r2 = pool
            .submit(
                cfg.clone(),
                false,
                Some(Box::new(move |n| p2.lock().unwrap().push(n))),
            )
            .unwrap();
        assert_eq!(
            pool.submit(cfg.clone(), false, None).unwrap_err(),
            Full { queued: 1 }
        );
        let r3 = pool.submit(cfg.clone(), true, None).unwrap();
        assert!(r1.wait(Duration::from_secs(30)).await.is_ok());
        assert!(r3.wait(Duration::from_secs(30)).await.is_ok());
        assert!(r2.wait(Duration::from_secs(30)).await.is_ok());
        assert_eq!(*places.lock().unwrap(), vec![1, 2, 1]);
        pool.stop();
    }
}
