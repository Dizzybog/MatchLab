//! Size-based resource budgeting for local ML (all native Rust).
//! MATCHLAB_ML_SIZE_GB is the *single* normal user-facing setting.
//! 0 disables ML, positive values enable it. The budget is shared between
//! evidence, result cache and pending training observations. Safe regex
//! validation, IMDb/AniDB identity matching and GitHub quotas never scale.
use serde_json::{json,Value};
use std::{env,sync::OnceLock};

const GIB:u64=1024*1024*1024;
const MAX_SIZE_GB:u64=128;
const MAX_EXAMPLES:u64=2_000_000;

#[derive(Clone,Debug,PartialEq,Eq)]
pub struct MlConfig{
    pub enabled:bool,
    pub size_gb:u64,
    pub max_examples:i64,
    pub queue_max_items:i64,
    pub cache_max_entries:i64,
    pub hot_cache_max_entries:usize,
    pub cache_write_queue:usize,
    pub training_batch_size:i64,
    pub learning_storage_bytes:u64,
    pub cache_storage_bytes:u64,
    pub queue_storage_bytes:u64,
    pub general_examples:i64,
    pub corrective_examples:i64,
    pub unknown_examples:i64,
    pub legacy_overrides:Vec<&'static str>,
}

fn read_number(name:&str,min:u64,max:u64)->Option<u64>{
    env::var(name).ok().and_then(|raw|raw.parse::<u64>().ok())
        .filter(|n|(*n>=min)&&(*n<=max))
}

impl MlConfig{
    /// Derive counts from a GiB budget. Count estimates are soft limits:
    /// each SQLite component also enforces its allocated byte budget.
    /// 50% evidence / 30% versioned cache / 20% history + pending queue.
    pub fn derive(size_gb:u64)->Self{
        let size=size_gb.min(MAX_SIZE_GB);
        let total=size.saturating_mul(GIB);
        let n=size.saturating_mul(50_000).clamp(100,MAX_EXAMPLES);
        let queue=n.saturating_mul(9).saturating_add(4)/5; // 1.8x
        let hot=(n/50).clamp(64,2048).min(n);
        let writer=(n/100).clamp(256,4096);
        let batch=if n<=20_000{16}else if n<=100_000{32}
            else if n<=500_000{64}else{96};
        Self{
            enabled:size>0,
            size_gb:size,
            max_examples:n as i64,
            queue_max_items:queue as i64,
            cache_max_entries:n as i64,
            hot_cache_max_entries:hot as usize,
            cache_write_queue:writer as usize,
            training_batch_size:batch,
            learning_storage_bytes:total*50/100,
            cache_storage_bytes:total*30/100,
            // Give integer-rounding remainder to the queue, so every
            // allocated byte is counted exactly once.
            queue_storage_bytes:total-total*50/100-total*30/100,
            general_examples:(n/6).clamp(1_024,131_072).min(n) as i64,
            corrective_examples:(n/100).clamp(128,16_384).min(n) as i64,
            unknown_examples:(n/8).clamp(256,262_144).min(n) as i64,
            legacy_overrides:Vec::new(),
        }
    }
    fn from_env()->Self{
        // The new setting is authoritative even when an old Compose file
        // still exports the historical individual capacity variables.
        if let Ok(raw)=env::var("MATCHLAB_ML_SIZE_GB"){
            let gb=raw.parse::<u64>().ok().filter(|n|*n<=MAX_SIZE_GB)
                .unwrap_or_else(||panic!(
                    "MATCHLAB_ML_SIZE_GB must be an integer from 0 to {MAX_SIZE_GB}"
                ));
            return Self::derive(gb);
        }
        // Backward-compatible legacy deployments: old ML_ENABLED and resource
        // overrides still work until the user opts into the new one-variable
        // setting. Their quotas may not share one total disk budget.
        let old_enabled=env::var("MATCHLAB_ML_ENABLED").ok()
            .is_some_and(|flag|flag.eq_ignore_ascii_case("true"));
        let old_gb=read_number("MATCHLAB_ML_MAX_STORAGE_GB",1,MAX_SIZE_GB)
            .unwrap_or(1);
        let mut c=Self::derive(old_gb);
        c.enabled=old_enabled;
        c.size_gb=if old_enabled{old_gb}else{0};
        let old_n=read_number("MATCHLAB_ML_TRAINING_MAX_EXAMPLES",100,MAX_EXAMPLES)
            .unwrap_or(12_000);
        c.max_examples=old_n as i64;
        c.general_examples=old_n.min(16_384) as i64;
        c.corrective_examples=1_024;
        c.unknown_examples=old_n.min(12_000) as i64;
        c.legacy_overrides.push("MATCHLAB_ML_ENABLED");
        c.legacy_overrides.push("MATCHLAB_ML_TRAINING_MAX_EXAMPLES");
        // Existing caps did not share a unified GB limit; retain historical
        // independent defaults for cache, queue and background batch.
        c.learning_storage_bytes=old_gb*GIB;
        c.queue_max_items=20_000;
        c.cache_max_entries=100_000;
        c.training_batch_size=64;
        c.cache_storage_bytes=GIB;
        c.queue_storage_bytes=0; // legacy had no history disk allocation
        for (key,min,max,slot) in [
            ("MATCHLAB_ML_QUEUE_MAX_ITEMS",1_000,3_600_000,0),
            ("MATCHLAB_ML_CACHE_MAX_ENTRIES",100,2_000_000,1),
            ("MATCHLAB_ML_TRAINING_BATCH_SIZE",8,128,2),
            ("MATCHLAB_ML_MAX_STORAGE_GB",1,MAX_SIZE_GB,3),
            ("MATCHLAB_ML_CACHE_MAX_STORAGE_GB",1,MAX_SIZE_GB,4),
        ]{
            if let Some(value)=read_number(key,min,max){
                match slot{
                    0=>c.queue_max_items=value as i64,
                    1=>c.cache_max_entries=value as i64,
                    2=>c.training_batch_size=value as i64,
                    3=>c.learning_storage_bytes=value*GIB,
                    4=>c.cache_storage_bytes=value*GIB,
                    _=>unreachable!(),
                }
                c.legacy_overrides.push(key);
            }
        }
        c
    }
}
static CONFIG:OnceLock<MlConfig>=OnceLock::new();
pub fn get()->&'static MlConfig{
    CONFIG.get_or_init(MlConfig::from_env)
}
pub fn status()->Value{
    let c=get();
    json!({
        "capacity_variable":"MATCHLAB_ML_SIZE_GB",
        "enabled":c.enabled,
        "size_gb":c.size_gb,
        "max_examples":c.max_examples,
        "queue_max_items":c.queue_max_items,
        "cache_max_entries":c.cache_max_entries,
        "hot_cache_max_entries":c.hot_cache_max_entries,
        "cache_write_queue":c.cache_write_queue,
        "training_batch_size":c.training_batch_size,
        "learning_storage_limit_mib":c.learning_storage_bytes/(1024*1024),
        "cache_storage_limit_mib":c.cache_storage_bytes/(1024*1024),
        "queue_storage_limit_mib":c.queue_storage_bytes/(1024*1024),
        "general_examples_limit":c.general_examples,
        "corrective_examples_limit":c.corrective_examples,
        "unknown_examples_limit":c.unknown_examples,
        "legacy_overrides":c.legacy_overrides,
        "storage_is_best_effort_not_filesystem_quota":true,
        "validation_and_regex_limits_scale":false
    })
}
#[cfg(test)]
mod tests{
    use super::*;
    #[test]
    fn size_scales_counts_and_partitioned_storage(){
        for (gb,n,queue,hot,writer) in [
            (1,50_000,90_000,1_000,500),
            (2,100_000,180_000,2_000,1_000),
            (10,500_000,900_000,2_048,4_096),
            (40,2_000_000,3_600_000,2_048,4_096),
        ]{
            let c=MlConfig::derive(gb);
            assert_eq!(c.size_gb,gb);
            assert!(c.enabled);
            assert_eq!(c.max_examples,n);
            assert_eq!(c.queue_max_items,queue);
            assert_eq!(c.cache_max_entries,n);
            assert_eq!(c.hot_cache_max_entries,hot);
            assert_eq!(c.cache_write_queue,writer);
            assert_eq!(c.learning_storage_bytes+c.cache_storage_bytes
                +c.queue_storage_bytes,gb*GIB);
            assert!(c.general_examples<=c.max_examples);
            assert!(c.corrective_examples<=c.max_examples);
        }
    }
    #[test]
    fn zero_disables_and_extremes_are_bounded(){
        let off=MlConfig::derive(0);
        let large=MlConfig::derive(u64::MAX);
        assert!(!off.enabled);
        assert_eq!(off.size_gb,0);
        assert_eq!(off.learning_storage_bytes,0);
        assert_eq!(large.size_gb,128);
        assert!(large.queue_max_items<=3_600_000);
        assert!(large.hot_cache_max_entries<=2048);
        assert!(large.cache_write_queue<=4096);
        assert!(large.training_batch_size<=128);
    }
}
