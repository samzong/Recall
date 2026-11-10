use std::collections::BTreeMap;

use serde::Serialize;

#[derive(Clone, Debug)]
pub struct Call {
    pub time: i64,
    pub stream: String,
    pub prompt: i64,
    pub out: i64,
    pub wrote: i64,
    pub wrote1: i64,
}

#[derive(Clone, Copy, Debug)]
pub struct Weights {
    pub read: f64,
    pub write: f64,
    pub write1h: f64,
    pub out: f64,
}

impl Weights {
    pub fn for_model(model: &str) -> Self {
        if model.starts_with("claude") {
            Self { read: 0.1, write: 1.25, write1h: 2.0, out: 5.0 }
        } else {
            Self { read: 0.1, write: 1.0, write1h: 1.0, out: 8.0 }
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct Point {
    pub value: i64,
    pub cost: f64,
    pub compacts: i64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Facts {
    pub calls: i64,
    pub streams: i64,
    pub compacts: i64,
    pub peak: i64,
    pub after: i64,
    pub growth: i64,
    pub rework: i64,
    pub hour: f64,
    pub sessions: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Compaction {
    pub current: i64,
    pub window: i64,
    pub best: i64,
    pub cost: f64,
    pub low: f64,
    pub compacts_current: i64,
    pub compacts_best: i64,
    pub curve: Vec<Point>,
    pub facts: Facts,
}

#[derive(Clone, Copy, Debug)]
pub struct Options {
    pub min: i64,
    pub max: i64,
    pub reserve: i64,
    pub current: i64,
    pub window: i64,
}

pub const MIN_CALLS: i64 = 50;
const STEP: i64 = 10_000;
const DROP_SHARE: f64 = 0.6;
const DROP_FROM: i64 = 20_000;
const REWORK_CALLS: usize = 10;
const SAME_COST: f64 = 0.03;

fn streams(calls: &[Call]) -> Vec<Vec<Call>> {
    let mut by: BTreeMap<&str, Vec<Call>> = BTreeMap::new();
    for call in calls {
        by.entry(call.stream.as_str()).or_default().push(call.clone());
    }
    by.into_values()
        .map(|mut stream| {
            stream.sort_by_key(|call| call.time);
            stream
        })
        .collect()
}

fn compacted(prev: i64, cur: i64) -> bool {
    prev >= DROP_FROM && (cur as f64) < DROP_SHARE * prev as f64
}

fn median(xs: &[i64]) -> i64 {
    if xs.is_empty() {
        return 0;
    }
    let mut sorted = xs.to_vec();
    sorted.sort_unstable();
    sorted[sorted.len() / 2]
}

fn observe(ss: &[Vec<Call>]) -> Facts {
    let mut facts = Facts::default();
    let (mut peaks, mut afters, mut growths, mut reworks) = (vec![], vec![], vec![], vec![]);
    for s in ss {
        facts.streams += 1;
        facts.calls += s.len() as i64;
        let mut ds = vec![];
        let mut cuts = vec![];
        for i in 1..s.len() {
            if compacted(s[i - 1].prompt, s[i].prompt) {
                peaks.push(s[i - 1].prompt);
                afters.push(s[i].prompt);
                cuts.push(i);
                continue;
            }
            let d = s[i].prompt - s[i - 1].prompt;
            if d >= 0 {
                ds.push(d);
            }
        }
        if !cuts.is_empty() {
            facts.sessions += 1;
        }
        let g = median(&ds);
        growths.extend_from_slice(&ds);
        for &at in &cuts {
            let end = at + REWORK_CALLS;
            if end >= s.len() {
                continue;
            }
            let extra = s[end].prompt - s[at].prompt - REWORK_CALLS as i64 * g;
            if (at + 1..=end).all(|j| !compacted(s[j - 1].prompt, s[j].prompt)) {
                reworks.push(extra.max(0));
            }
        }
    }
    facts.compacts = peaks.len() as i64;
    facts.peak = median(&peaks);
    facts.after = median(&afters);
    facts.growth = median(&growths);
    facts.rework = median(&reworks);
    facts
}

fn base(ss: &[Vec<Call>]) -> i64 {
    let firsts: Vec<i64> = ss.iter().map(|s| s[0].prompt).collect();
    median(&firsts)
}

struct Replay {
    after: i64,
    base: i64,
    rework: i64,
    weights: Weights,
    write: f64,
}

impl Replay {
    fn run(&self, ss: &[Vec<Call>], threshold: i64) -> (f64, i64) {
        let w = self.weights;
        let (mut cost, mut n) = (0.0, 0);
        let summary = (self.after - self.base).max(0);
        for s in ss {
            let mut ctx = s[0].prompt;
            cost += ctx as f64 * self.write + s[0].out as f64 * w.out;
            for i in 1..s.len() {
                let mut d = s[i].prompt - s[i - 1].prompt;
                if d < 0 || compacted(s[i - 1].prompt, s[i].prompt) {
                    d = 0;
                }
                if ctx + d > threshold && ctx > self.after {
                    cost += ctx as f64 * w.read
                        + summary as f64 * w.out
                        + (summary + self.rework) as f64 * self.write;
                    ctx = self.after + self.rework;
                    n += 1;
                }
                cost += ctx as f64 * w.read + d as f64 * self.write + s[i].out as f64 * w.out;
                ctx += d;
            }
        }
        (cost, n)
    }
}

pub fn advise(calls: &[Call], weights: Weights, o: Options) -> Option<Compaction> {
    let ss = streams(calls);
    let mut facts = observe(&ss);
    if facts.calls < MIN_CALLS || facts.streams == 0 {
        return None;
    }
    let mut write = weights.write;
    let wrote: i64 = calls.iter().map(|c| c.wrote).sum();
    let wrote1: i64 = calls.iter().map(|c| c.wrote1).sum();
    if wrote > 0 {
        facts.hour = wrote1 as f64 / wrote as f64;
        if facts.hour > 0.5 {
            write = weights.write1h;
        }
    }
    let b = base(&ss);
    let after = if facts.after == 0 { b + 10_000 } else { facts.after };
    let rework = if facts.compacts < 5 { facts.rework.max(10_000) } else { facts.rework };
    let mut cur = o.current;
    if cur == 0 {
        cur = if facts.compacts >= 3 {
            round_to(facts.peak + o.reserve)
        } else if o.window > 0 {
            o.window
        } else {
            o.max
        };
    }
    let mut top = o.max;
    if o.window > 0 {
        cur = cur.min(o.window);
        top = o.max.min(o.window);
    }
    let floor = o.min.max(round_up(after + (after / 2).max(20 * facts.growth) + o.reserve));
    let replay = Replay { after, base: b, rework, weights, write };
    let point = |v: i64| {
        let (cost, compacts) = replay.run(&ss, (after + 1).max(v - o.reserve));
        Point { value: v, cost, compacts }
    };
    let now = point(cur);
    let mut curve = vec![];
    let mut v = floor;
    while v <= top {
        curve.push(point(v));
        v += STEP;
    }
    let mut low = *curve.first()?;
    for p in &curve {
        if p.cost < low.cost {
            low = *p;
        }
    }
    let mut best = low;
    for p in &curve {
        if p.cost <= low.cost * 1.01 && p.value > best.value {
            best = *p;
        }
    }
    if now.cost <= 0.0 || (now.cost - best.cost) / now.cost < SAME_COST {
        best = now;
    }
    Some(Compaction {
        current: cur,
        window: o.window,
        best: best.value,
        cost: now.cost,
        low: best.cost,
        compacts_current: now.compacts,
        compacts_best: best.compacts,
        curve,
        facts,
    })
}

fn round_to(v: i64) -> i64 {
    (v + STEP / 2) / STEP * STEP
}

fn round_up(v: i64) -> i64 {
    (v + STEP - 1) / STEP * STEP
}
