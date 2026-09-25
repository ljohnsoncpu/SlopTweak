//! Offer filtering and ranking.
//!
//! Price alone is misleading: bandwidth is billed per GB and varies ~15x
//! between hosts (findings, Phase 1), so a cheap GPU on an expensive link can
//! cost more for a short session than a pricier GPU. We rank by the expected
//! cost of the whole session:
//!
//! `dph_total * hours + storage * hours + model_GB * inet_down_cost`

use std::collections::HashSet;

use serde::Serialize;

use super::{Offer, OfferQuery};

const HOURS_PER_MONTH: f64 = 730.0;
const BYTES_PER_GB: f64 = 1e9;

#[derive(Debug, Clone, PartialEq)]
pub struct CostInputs {
    pub expected_hours: f64,
    pub model_bytes: u64,
    pub disk_gb: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RankedOffer {
    pub offer: Offer,
    /// Hourly price including storage.
    pub hourly: f64,
    /// One-off model download cost.
    pub download_cost: f64,
    /// Expected cost of the session; the ranking key.
    pub expected_cost: f64,
}

pub fn passes(offer: &Offer, q: &OfferQuery) -> bool {
    offer.verified
        && offer.num_gpus == 1
        && offer.reliability >= q.min_reliability
        && offer.inet_down_mbps >= q.min_inet_down_mbps
        // Vast reports MB; 12 GB cards show up as ~12,000-12,288.
        && offer.gpu_ram_mb >= q.min_vram_gb * 1000.0
        && offer.dph_total <= q.max_dph
        && offer.disk_space_gb >= q.min_disk_gb
        && offer.cuda_max_good >= q.min_cuda
        && offer.dph_total.is_finite()
        && offer.inet_down_cost.is_finite()
        && offer.inet_down_cost >= 0.0
}

pub fn cost(offer: &Offer, c: &CostInputs) -> RankedOffer {
    let storage_hourly = offer.storage_cost.max(0.0) * c.disk_gb / HOURS_PER_MONTH;
    let hourly = offer.dph_total + storage_hourly;
    let download_cost = c.model_bytes as f64 / BYTES_PER_GB * offer.inet_down_cost;
    RankedOffer {
        offer: offer.clone(),
        hourly,
        download_cost,
        expected_cost: hourly * c.expected_hours + download_cost,
    }
}

/// Offers and machines already tried this session. A machine that failed
/// once is skipped entirely: it often lists several offers (seen live: two
/// offers on one stuck host cost two 8-minute timeouts).
#[derive(Debug, Default, Clone)]
pub struct Tried {
    offers: HashSet<u64>,
    machines: HashSet<u64>,
}

impl Tried {
    pub fn add(&mut self, o: &Offer) {
        self.offers.insert(o.id);
        if let Some(m) = o.machine_id {
            self.machines.insert(m);
        }
    }

    pub fn contains(&self, o: &Offer) -> bool {
        self.offers.contains(&o.id) || o.machine_id.is_some_and(|m| self.machines.contains(&m))
    }
}

/// Filter, drop already-tried offers/machines, and sort cheapest expected
/// session first. Ties go to the more reliable host.
pub fn rank(offers: &[Offer], q: &OfferQuery, c: &CostInputs, tried: &Tried) -> Vec<RankedOffer> {
    let mut ranked: Vec<RankedOffer> = offers
        .iter()
        .filter(|o| !tried.contains(o) && passes(o, q))
        .map(|o| cost(o, c))
        .collect();
    ranked.sort_by(|a, b| {
        a.expected_cost
            .total_cmp(&b.expected_cost)
            .then(b.offer.reliability.total_cmp(&a.offer.reliability))
            .then(a.offer.id.cmp(&b.offer.id))
    });
    ranked
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offer(id: u64, dph: f64, down_cost: f64) -> Offer {
        Offer {
            id,
            gpu_name: "RTX A4000".into(),
            gpu_ram_mb: 16376.0,
            num_gpus: 1,
            dph_total: dph,
            storage_cost: 0.2,
            inet_down_cost: down_cost,
            inet_down_mbps: 4000.0,
            reliability: 0.995,
            verified: true,
            disk_space_gb: 100.0,
            cuda_max_good: 12.8,
            geolocation: Some("US".into()),
            machine_id: Some(id + 1000),
        }
    }

    fn query() -> OfferQuery {
        OfferQuery {
            min_vram_gb: 12.0,
            min_disk_gb: 50.0,
            min_reliability: 0.98,
            min_inet_down_mbps: 2000.0,
            max_dph: 0.5,
            min_cuda: 12.4,
            limit: 64,
        }
    }

    fn inputs() -> CostInputs {
        CostInputs {
            expected_hours: 1.0,
            model_bytes: 6_938_043_264,
            disk_gb: 50.0,
        }
    }

    #[test]
    fn bandwidth_can_outweigh_hourly_price() {
        // Live example: $0.0825/hr at $0.039/GB vs $0.1076/hr at $0.0026/GB.
        let cheap_gpu_pricey_link = offer(1, 0.0825, 0.0390625);
        let pricier_gpu_cheap_link = offer(2, 0.1076, 0.0026041666);
        let r = rank(
            &[cheap_gpu_pricey_link, pricier_gpu_cheap_link],
            &query(),
            &inputs(),
            &Tried::default(),
        );
        assert_eq!(r[0].offer.id, 2);
        assert!((r[1].download_cost - 0.271).abs() < 0.001);
    }

    #[test]
    fn long_sessions_favour_hourly_price() {
        let mut c = inputs();
        c.expected_hours = 20.0;
        let r = rank(
            &[offer(1, 0.0825, 0.0390625), offer(2, 0.1076, 0.0026041666)],
            &query(),
            &c,
            &Tried::default(),
        );
        assert_eq!(r[0].offer.id, 1);
    }

    #[test]
    fn storage_is_included_in_hourly() {
        let r = cost(&offer(1, 0.1, 0.0), &inputs());
        // 0.2 $/GB/month * 50 GB / 730 h
        assert!((r.hourly - (0.1 + 0.2 * 50.0 / 730.0)).abs() < 1e-9);
        assert_eq!(r.download_cost, 0.0);
    }

    #[test]
    fn filters_apply() {
        let q = query();
        let base = offer(1, 0.1, 0.0);
        assert!(passes(&base, &q));
        type Mutation = Box<dyn Fn(&mut Offer)>;
        let cases: Vec<(&str, Mutation)> = vec![
            ("unverified", Box::new(|o| o.verified = false)),
            ("multi-gpu", Box::new(|o| o.num_gpus = 2)),
            ("unreliable", Box::new(|o| o.reliability = 0.97)),
            ("slow link", Box::new(|o| o.inet_down_mbps = 1999.0)),
            ("small vram", Box::new(|o| o.gpu_ram_mb = 8192.0)),
            ("too pricey", Box::new(|o| o.dph_total = 0.51)),
            ("small disk", Box::new(|o| o.disk_space_gb = 40.0)),
            ("old cuda", Box::new(|o| o.cuda_max_good = 12.2)),
            ("nan price", Box::new(|o| o.dph_total = f64::NAN)),
            ("negative bw", Box::new(|o| o.inet_down_cost = -1.0)),
        ];
        for (name, mutate) in cases {
            let mut o = base.clone();
            mutate(&mut o);
            assert!(!passes(&o, &q), "{name} should be filtered");
        }
        // 12 GB cards report ~12,288 MB; must pass a 12 GB minimum.
        let mut o = base.clone();
        o.gpu_ram_mb = 12288.0;
        assert!(passes(&o, &q));
    }

    #[test]
    fn excluded_offers_are_skipped() {
        let offers = [offer(1, 0.1, 0.0), offer(2, 0.2, 0.0)];
        let mut tried = Tried::default();
        tried.add(&offers[0]);
        let r = rank(&offers, &query(), &inputs(), &tried);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].offer.id, 2);
    }

    #[test]
    fn failed_machine_is_skipped_across_its_offers() {
        let a = offer(1, 0.1, 0.0);
        let mut same_machine = offer(2, 0.1, 0.0);
        same_machine.machine_id = a.machine_id;
        let other = offer(3, 0.2, 0.0);
        let mut tried = Tried::default();
        tried.add(&a);
        let r = rank(&[a, same_machine, other], &query(), &inputs(), &tried);
        assert_eq!(r.iter().map(|r| r.offer.id).collect::<Vec<_>>(), vec![3]);
    }

    #[test]
    fn ties_prefer_reliability() {
        let mut a = offer(1, 0.1, 0.0);
        let mut b = offer(2, 0.1, 0.0);
        a.reliability = 0.981;
        b.reliability = 0.999;
        let r = rank(&[a, b], &query(), &inputs(), &Tried::default());
        assert_eq!(r[0].offer.id, 2);
    }
}
