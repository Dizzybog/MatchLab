//! Bounded, single-flight cache for generation-scoped query results.
//! Reuse immutable candidate sets between files; when holding a finished
//! diagnostic, the caller MUST key it by the complete original path,
//! per-request overrides and current parser/metadata generation.
use std::{
    collections::{HashMap,VecDeque},
    sync::{Arc,Mutex},
    time::{Duration,Instant},
};

// Share finished responses across threads without cloning large candidate
// vectors while holding the process-wide cache mutex.
struct Ready<T> { at:Instant, sequence:u64, value:Arc<T> }
struct Inner<T> {
    ready:HashMap<String,Ready<T>>,
    // Exact insertion order for O(1)-amortized eviction. Sequence tags
    // distinguish stale tickets after a key expires and is reinserted.
    eviction:VecDeque<(String,u64)>,
    next_sequence:u64,
    // One short lock per key, so distinct titles are never serialized while
    // simultaneous requests for a single torrent share the same lookup.
    flights:HashMap<String,Arc<Mutex<()>>>,
}

pub struct QueryCache<T> {
    inner:Mutex<Inner<T>>,
    max_entries:usize,
    ttl:Duration,
}

// Keep a key's lock registered while any computation or waiter still owns it.
// Removing it when the first computation finishes lets a new request create a
// second lock alongside waiters retrying an error or an uncacheable result.
struct Flight<'a,T> {
    cache:&'a QueryCache<T>,
    key:String,
    lock:Arc<Mutex<()>>,
}
impl<T> Drop for Flight<'_,T> {
    fn drop(&mut self){
        if let Ok(mut inner)=self.cache.inner.lock(){
            // The registry and this request are the final two owners. The
            // cache mutex prevents a new waiter from joining during removal.
            if Arc::strong_count(&self.lock)==2
                && inner.flights.get(&self.key)
                    .is_some_and(|lock|Arc::ptr_eq(lock,&self.lock))
            {
                inner.flights.remove(&self.key);
            }
        }
    }
}
impl<T:Clone> QueryCache<T> {
    pub fn new(max_entries:usize,ttl:Duration)->Self{
        Self{inner:Mutex::new(Inner{
                ready:HashMap::new(),eviction:VecDeque::new(),
                next_sequence:0,flights:HashMap::new()}),
            max_entries,ttl}
    }

    pub fn get_or_compute(
        &self,key:String,compute:impl FnOnce()->Result<T,String>,
        cacheable:impl Fn(&T)->bool,
    )->Result<T,String>{
        // Fast path: only clone a completed result while holding the cache
        // mutex. Never perform Tantivy or SQLite work under this global lock.
        let (cached,flight)={
            let mut inner=self.inner.lock().map_err(|e|e.to_string())?;
            let cached=inner.ready.get(&key)
                .filter(|entry|entry.at.elapsed()<self.ttl)
                .map(|entry|Arc::clone(&entry.value));
            let flight=if cached.is_some(){
                None
            }else{
                inner.ready.remove(&key);
                Some(Arc::clone(inner.flights.entry(key.clone()).or_insert_with(||
                    Arc::new(Mutex::new(())))))
            };
            (cached,flight)
        };
        // Clone the payload after releasing the global mutex.
        if let Some(value)=cached{return Ok((*value).clone());}
        let flight=Flight{cache:self,key:key.clone(),
            lock:flight.expect("cache miss has a single-flight lock")};
        let _guard=flight.lock.lock().map_err(|e|e.to_string())?;
        // Another request may have populated this entry while we waited.
        let cached={
            let inner=self.inner.lock().map_err(|e|e.to_string())?;
            inner.ready.get(&key)
                .filter(|entry|entry.at.elapsed()<self.ttl)
                .map(|entry|Arc::clone(&entry.value))
        };
        if let Some(value)=cached{return Ok((*value).clone());}
        let result=compute();
        // The potentially expensive T::clone also happens off-lock.
        let cache_copy=match &result {
            Ok(value) if self.max_entries>0 && cacheable(value)=>
                Some(Arc::new(value.clone())),
            _=>None,
        };
        // Do not cache transient query errors. Limit the number of entries and
        // evict expired/oldest results to avoid unbounded memory growth.
        if let Ok(mut inner)=self.inner.lock(){
            if let Some(value)=cache_copy {
                if inner.ready.len()>=self.max_entries {
                    // The oldest live entry is first. Skip stale tickets
                    // from invalidated/replaced cache entries in O(1).
                    while let Some((old_key,sequence))=inner.eviction.pop_front(){
                        if inner.ready.get(&old_key)
                            .is_some_and(|entry|entry.sequence==sequence){
                            inner.ready.remove(&old_key);
                            break;
                        }
                    }
                }
                inner.next_sequence=inner.next_sequence.wrapping_add(1);
                let sequence=inner.next_sequence;
                inner.ready.insert(key.clone(),
                    Ready{at:Instant::now(),sequence,value});
                inner.eviction.push_back((key.clone(),sequence));
                // Repeated expiration/reinsertion of the same key leaves
                // stale tickets; compact infrequently to cap queue memory.
                if inner.eviction.len()>self.max_entries.saturating_mul(2).max(8){
                    let mut live=inner.ready.iter()
                        .map(|(key,entry)|(entry.at,key.clone(),entry.sequence))
                        .collect::<Vec<_>>();
                    live.sort_unstable_by_key(|(at,_,_)|*at);
                    inner.eviction=live.into_iter().map(|(_,key,seq)|(key,seq)).collect();
                }
            }
        }
        result
    }
}

#[cfg(test)]
mod tests{
    use super::*;
    use std::sync::atomic::{AtomicUsize,Ordering};

    #[test]
    fn cache_payload_clones_never_hold_the_global_mutex(){
        struct Probe{cache:std::sync::Weak<QueryCache<Probe>>}
        impl Clone for Probe{
            fn clone(&self)->Self{
                if let Some(cache)=self.cache.upgrade(){
                    assert!(cache.inner.try_lock().is_ok(),
                        "copying a large cached response must not serialize other titles");
                }
                Self{cache:self.cache.clone()}
            }
        }
        let cache=Arc::new(QueryCache::<Probe>::new(8,Duration::from_secs(60)));
        let probe=Probe{cache:Arc::downgrade(&cache)};
        // Insertion and retrieval both invoke Clone, neither under the lock.
        cache.get_or_compute("probe".into(),||Ok(probe),|_|true).unwrap();
        cache.get_or_compute("probe".into(),||{
            panic!("valid cache entry must suppress duplicate computation")
        },|_|true).unwrap();
    }

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
    fn waiters_keep_their_flight_registered_after_uncacheable_results(){
        use std::sync::mpsc;
        // Cover errors, explicitly uncacheable payloads, disabled storage,
        // and results that expire before the next waiter acquires the lock.
        for (fail,cacheable,capacity,ttl) in [
            (true,true,8,Duration::from_secs(60)),
            (false,false,8,Duration::from_secs(60)),
            (false,true,0,Duration::from_secs(60)),
            (false,true,8,Duration::ZERO),
        ] {
            let cache=Arc::new(QueryCache::<usize>::new(capacity,ttl));
            let (first_entered,first_ready)=mpsc::channel();
            let (release_first,first_release)=mpsc::channel();
            let first_cache=cache.clone();
            let first=std::thread::spawn(move||first_cache.get_or_compute(
                "same".into(),||{
                    first_entered.send(()).unwrap();
                    first_release.recv().unwrap();
                    if fail{Err("transient failure".into())}else{Ok(1)}
                },|_|cacheable));
            first_ready.recv_timeout(Duration::from_secs(5)).unwrap();
            let (second_entered,second_ready)=mpsc::channel();
            let (release_second,second_release)=mpsc::channel();
            let second_cache=cache.clone();
            let second=std::thread::spawn(move||second_cache.get_or_compute(
                "same".into(),||{
                    second_entered.send(()).unwrap();
                    second_release.recv().unwrap();
                    Ok(2)
                },|_|cacheable));
            let deadline=Instant::now()+Duration::from_secs(5);
            // Wait until the second request has joined this exact flight.
            loop {
                let joined=cache.inner.lock().unwrap().flights.get("same")
                    .is_some_and(|lock|Arc::strong_count(lock)>=3);
                if joined{break;}
                assert!(Instant::now()<deadline,"second request did not join flight");
                std::thread::yield_now();
            }
            release_first.send(()).unwrap();
            assert_eq!(first.join().unwrap().is_err(),fail);
            second_ready.recv_timeout(Duration::from_secs(5)).unwrap();
            // A later arrival must find the lock held by the second request,
            // rather than create another concurrently executing flight.
            let registered=cache.inner.lock().unwrap().flights.get("same")
                .is_some_and(|lock|lock.try_lock().is_err());
            release_second.send(()).unwrap();
            assert_eq!(second.join().unwrap().unwrap(),2);
            assert!(registered,"running waiter lost its registered flight");
            assert!(cache.inner.lock().unwrap().flights.is_empty(),
                "the final request must release the flight registry entry");
        }
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
    fn unique_key_churn_keeps_eviction_index_bounded_and_evicts_oldest(){
        let cache=QueryCache::<usize>::new(12,Duration::from_secs(60));
        for n in 0..400 {
            let key=format!("unique-{n}");
            assert_eq!(cache.get_or_compute(key,||Ok(n),|_|true).unwrap(),n);
        }
        let inner=cache.inner.lock().unwrap();
        assert_eq!(inner.ready.len(),12);
        assert!(inner.eviction.len()<=24);
        for n in 0..388 {assert!(!inner.ready.contains_key(&format!("unique-{n}")));}
        for n in 388..400 {assert!(inner.ready.contains_key(&format!("unique-{n}")));}
    }

    #[test]
    fn repeated_expired_key_does_not_accumulate_stale_eviction_tickets(){
        let cache=QueryCache::<usize>::new(4,Duration::ZERO);
        for n in 0..100 {
            assert_eq!(cache.get_or_compute("same".to_owned(),||Ok(n),|_|true).unwrap(),n);
        }
        let inner=cache.inner.lock().unwrap();
        assert_eq!(inner.ready.len(),1);
        assert!(inner.eviction.len()<=8,"stale eviction records must be compacted");
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
