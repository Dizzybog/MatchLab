//! Centralized capacity policy for the optional background ML subsystem.
//! One user-facing knob controls retained evidence, queues, caches and storage.
//! Legacy per-component limits remain opt-in overrides for existing deployments.
//! Safety thresholds (regex validation, GitHub upload rate, CPU concurrency)
//! deliberately do not scale with the training corpus.
use serde_json::{json,Value};
use std::{env,sync::OnceLock};

const DEFAULT_EXAMPLES:u64=100_000;
const MAX_EXAMPLES:u64=2_000_000;
const GIB:u64=1024*1024*1024;

#[derive(Clone,Debug,PartialEq,Eq)]
pub struct MlConfig{
    pub max_examples:i64,
    pub queue_max_items:i64,
    pub cache_max_entries:i64,
    pub hot_cache_max_entries:usize,
    pub cache_write_queue:usize,
    pub training_batch_size:i64,
    pub learning_storage_gb:u64,
    pub cache_storage_gb:u64,
    pub general_examples:i64,
    pub corrective_examples:i64,
    pub unknown_examples:i64,
    pub legacy_overrides:Vec<&'static str>,
}
fn read_number(name:&str,min:u64,max:u64)->Option<u64>{
    env::var(name).ok().and_then(|raw|raw.parse::<u64>().ok())
        .filter(|n|(*n>=min)&&(*n<=max))
}
fn gib_for(examples:u64,bytes_per_example:u64)->u64{
    examples.saturating_mul(bytes_per_example)
        .saturating_add(GIB-1).saturating_div(GIB).clamp(1,128)
}
impl MlConfig{
    /// Pure derivation lets the scaling contract be tested without mutating
    /// process-global environment variables or involving a background worker.
    pub fn derive(requested:u64)->Self{
        let n=requested.clamp(100,MAX_EXAMPLES);
        let queue=n.saturating_mul(9).saturating_add(4)/5; // ceil(N * 1.8)
        let hot=(n/50).clamp(64,2048).min(n);
        let writer=(n/100).clamp(256,4096);
        let batch=if n<=20_000{16}else if n<=100_000{32}
            else if n<=500_000{64}else{96};
        Self{
            max_examples:n as i64,
            queue_max_items:queue as i64,
            cache_max_entries:n as i64,
            hot_cache_max_entries:hot as usize,
            cache_write_queue:writer as usize,
            training_batch_size:batch,
            // Conservative page/indices headroom estimates, not guaranteed
            // SQLite file sizes. Quota maintenance measures actual used pages.
            learning_storage_gb:gib_for(n,12*1024),
            cache_storage_gb:gib_for(n,4*1024),
            general_examples:(n/6).clamp(1_024,131_072).min(n) as i64,
            corrective_examples:(n/100).clamp(128,16_384).min(n) as i64,
            unknown_examples:(n/8).clamp(256,262_144).min(n) as i64,
            legacy_overrides:Vec::new(),
        }
    }
    fn from_env()->Self{
        let n=read_number("MATCHLAB_ML_MAX_EXAMPLES",100,MAX_EXAMPLES)
            .or_else(||read_number("MATCHLAB_ML_TRAINING_MAX_EXAMPLES",100,MAX_EXAMPLES))
            .unwrap_or(DEFAULT_EXAMPLES);
        let mut config=Self::derive(n);
        // Explicit legacy options take precedence for compatibility. New
        // deployments should expose only MATCHLAB_ML_MAX_EXAMPLES.
        for (key,min,max,slot) in [
            ("MATCHLAB_ML_QUEUE_MAX_ITEMS",100,3_600_000,0),
            ("MATCHLAB_ML_CACHE_MAX_ENTRIES",100,2_000_000,1),
            ("MATCHLAB_ML_TRAINING_BATCH_SIZE",8,128,2),
            ("MATCHLAB_ML_MAX_STORAGE_GB",1,128,3),
            ("MATCHLAB_ML_CACHE_MAX_STORAGE_GB",1,128,4),
        ]{
            if let Some(value)=read_number(key,min,max){
                match slot{
                    0=>config.queue_max_items=value as i64,
                    1=>config.cache_max_entries=value as i64,
                    2=>config.training_batch_size=value as i64,
                    3=>config.learning_storage_gb=value,
                    4=>config.cache_storage_gb=value,
                    _=>unreachable!(),
                }
                config.legacy_overrides.push(key);
            }
        }
        config
    }
}
static CONFIG:OnceLock<MlConfig>=OnceLock::new();
pub fn get()->&'static MlConfig{
    CONFIG.get_or_init(MlConfig::from_env)
}
pub fn status()->Value{
    let c=get();
    json!({
        "capacity_variable":"MATCHLAB_ML_MAX_EXAMPLES",
        "max_examples":c.max_examples,
        "queue_max_items":c.queue_max_items,
        "cache_max_entries":c.cache_max_entries,
        "hot_cache_max_entries":c.hot_cache_max_entries,
        "cache_write_queue":c.cache_write_queue,
        "training_batch_size":c.training_batch_size,
        "learning_storage_limit_gb":c.learning_storage_gb,
        "cache_storage_limit_gb":c.cache_storage_gb,
        "general_examples_limit":c.general_examples,
        "corrective_examples_limit":c.corrective_examples,
        "unknown_examples_limit":c.unknown_examples,
        "legacy_overrides":c.legacy_overrides,
        "storage_budgets_are_estimates":true,
        "validation_and_regex_limits_scale":false
    })
}
#[cfg(test)]
mod tests{
    use super::*;
    #[test]
    fn recommended_profiles(){
        for (n,queue,cache,hot,writer) in [
            (10_000,18_000,10_000,200,256),
            (50_000,90_000,50_000,1_000,500),
            (100_000,180_000,100_000,2_000,1_000),
            (500_000,900_000,500_000,2_048,4_096),
        ]{
            let c=MlConfig::derive(n);
            assert_eq!(c.max_examples,n as i64);
            assert_eq!(c.queue_max_items,queue);
            assert_eq!(c.cache_max_entries,cache);
            assert_eq!(c.hot_cache_max_entries,hot);
            assert_eq!(c.cache_write_queue,writer);
            assert!(c.general_examples<=c.max_examples);
            assert!(c.corrective_examples<=c.max_examples);
        }
    }
    #[test]
    fn capacities_are_bounded_even_at_extremes(){
        let small=MlConfig::derive(1);
        let large=MlConfig::derive(u64::MAX);
        assert_eq!(small.max_examples,100);
        assert_eq!(large.max_examples,2_000_000);
        assert!(large.queue_max_items<=3_600_000);
        assert!(large.hot_cache_max_entries<=2048);
        assert!(large.cache_write_queue<=4096);
        assert!(large.training_batch_size<=128);
        assert!(large.learning_storage_gb<=128);
        assert!(large.cache_storage_gb<=128);
    }
}