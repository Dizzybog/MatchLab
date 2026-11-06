//! Bounded, single-flight cache for immutable metadata search results.
//! This intentionally caches lookup *candidates*, not the final per-file SID
//! or ASID: season/episode coordinates must always be resolved per request.
use std::{
    collections::HashMap,
    sync::{Arc,Mutex},
    time::{Duration,Instant},
};

struct Ready<T> { at:Instant, value:T }
struct Inner<T> {
    ready:HashMap<String,Ready<T>>,
    // One short lock per key, so distinct titles are never serialized while
    // simultaneous requests for a single torrent share the same lookup.
    flights:HashMap<String,Arc<Mutex<()>>>,
}

pub struct QueryCache<T> {
    inner:Mutex<Inner<T>>,
    max_entries:usize,
    ttl:Duration,
}
impl<T:Clone> QueryCache<T> {
    pub fn new(max_entries:usize,ttl:Duration)->Self{
        Self{inner:Mutex::new(Inner{ready:HashMap::new(),flights:HashMap::new()}),
            max_entries,ttl}
    }

    pub fn get_or_compute(
        &self,key:String,compute:impl FnOnce()->Result<T,String>,
        cacheable:impl Fn(&T)->bool,
    )->Result<T,String>{
        // Fast path: only clone a completed result while holding the cache
        // mutex. Never perform Tantivy or SQLite work under this global lock.
        let flight={
            let mut inner=self.inner.lock().map_err(|e|e.to_string())?;
            if let Some(entry)=inner.ready.get(&key){
                if entry.at.elapsed()<self.ttl{return Ok(entry.value.clone());}
            }
            inner.ready.remove(&key);
            Arc::clone(inner.flights.entry(key.clone()).or_insert_with(||
                Arc::new(Mutex::new(()))))
        };
        let _guard=flight.lock().map_err(|e|e.to_string())?;
        // A different request may have populated this entry while we waited.
        {
            let inner=self.inner.lock().map_err(|e|e.to_string())?;
            if let Some(entry)=inner.ready.get(&key){
                if entry.at.elapsed()<self.ttl{return Ok(entry.value.clone());}
            }
        }
        let result=compute();
        // Do not cache transient query errors. Limit the number of entries and
        // evict expired/oldest results to avoid unbounded memory growth.
        if let Ok(mut inner)=self.inner.lock(){
            if let Ok(ref value)=result {
                if self.max_entries>0 && cacheable(value){
                    if inner.ready.len()>=self.max_entries{
                        inner.ready.retain(|_,entry|entry.at.elapsed()<self.ttl);
                    }
                    if inner.ready.len()>=self.max_entries {
                        if let Some(oldest)=inner.ready.iter().min_by_key(|(_,e)|e.at)
                            .map(|(key,_)|key.clone()) {
                            inner.ready.remove(&oldest);
                        }
                    }
                    inner.ready.insert(key.clone(),Ready{at:Instant::now(),value:value.clone()});
                }
            }
            inner.flights.remove(&key);
        }
        result
    }
}

#[cfg(test)]
mod tests{
    use super::*;
    use std::sync::atomic::{AtomicUsize,Ordering};

    #[test]
    fn same_key_hits_cache_but_different_key_is_independent(){
        let cache=QueryCache::<String>::new(2,Duration::from_secs(60));
        let n=AtomicUsize::new(0);
        for key in ["torrent:doctorwho","torrent:doctorwho","torrent:onepiece"]{
            let response=cache.get_or_compute(key.to_string(),||{
                n.fetch_add(1,Ordering::SeqCst);
                Ok(key.to_owned())
            },|_|true).unwrap();
            assert_eq!(response,key);
        }
        assert_eq!(n.load(Ordering::SeqCst),2);
    }

    #[test]
    fn simultaneous_requests_for_one_title_perform_one_lookup(){
        let cache=Arc::new(QueryCache::<usize>::new(16,Duration::from_secs(60)));
        let n=Arc::new(AtomicUsize::new(0));
        let barrier=Arc::new(std::sync::Barrier::new(16));
        let mut jobs=Vec::new();
        for _ in 0..16{
            let cache=cache.clone();
            let n=n.clone();
            let barrier=barrier.clone();
            jobs.push(std::thread::spawn(move||{
                barrier.wait();
                cache.get_or_compute("same:source-generation:anime".into(),||{
                    n.fetch_add(1,Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(12));
                    Ok(42)
                },|_|true).unwrap()
            }));
        }
        for job in jobs{assert_eq!(job.join().unwrap(),42);}
        assert_eq!(n.load(Ordering::SeqCst),1);
    }

    #[test]
    fn errors_and_oversized_values_are_not_reused(){
        let cache=QueryCache::<Vec<u32>>::new(2,Duration::from_secs(60));
        assert!(cache.get_or_compute("a".into(),||Err("failure".into()),|_|true).is_err());
        assert_eq!(cache.get_or_compute("a".into(),||Ok(vec![1]),|_|true).unwrap(),vec![1]);
        let n=AtomicUsize::new(0);
        for _ in 0..2 {
            cache.get_or_compute("huge".into(),||{
                n.fetch_add(1,Ordering::SeqCst);
                Ok(vec![7;1024])
            },|v|v.len()<=100).unwrap();
        }
        assert_eq!(n.load(Ordering::SeqCst),2);
    }

    #[test]
    fn expiration_and_capacity_are_enforced(){
        let cache=QueryCache::<usize>::new(1,Duration::from_millis(1));
        cache.get_or_compute("a".into(),||Ok(1),|_|true).unwrap();
        cache.get_or_compute("b".into(),||Ok(2),|_|true).unwrap();
        assert_eq!(cache.inner.lock().unwrap().ready.len(),1);
        std::thread::sleep(Duration::from_millis(4));
        let n=AtomicUsize::new(0);
        cache.get_or_compute("b".into(),||{
            n.fetch_add(1,Ordering::SeqCst);
            Ok(3)
        },|_|true).unwrap();
        assert_eq!(n.load(Ordering::SeqCst),1);
    }
}
