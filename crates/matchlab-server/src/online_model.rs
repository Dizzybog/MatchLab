//! Bounded, genuinely trainable online logistic ranker using FTRL-Proximal.
//! All computation, training, persistence, and inference are native Rust.
//! It never approves rules or changes IMDb/AniDB identities; the score is
//! shadow-only prioritization for independently checked field observations.
//! Training labels come solely from original parser or allowlisted literal
//! evidence; counterfactual *different field* labels are explicit negatives.
use rusqlite::{params,Connection,OptionalExtension};
use sha2::{Digest,Sha256};
use serde_json::{json,Value};
use std::sync::atomic::{AtomicI64,Ordering};
use std::collections::BTreeSet;

const EPOCH:i64=2;
fn current_build()->String{
    format!("{}:{}",option_env!("MATCHLAB_BUILD_SHA").unwrap_or("dev"),EPOCH)
}
const DIM:usize=1<<14;
const MAX_BYTES:usize=DIM*8;
const MAX_EXAMPLES:i64=10_000_000;

static POSITIVE:AtomicI64=AtomicI64::new(0);
static NEGATIVE:AtomicI64=AtomicI64::new(0);
static HOLDOUT:AtomicI64=AtomicI64::new(0);
static HOLDOUT_CORRECT:AtomicI64=AtomicI64::new(0);
static RECENT_HOLDOUT:AtomicI64=AtomicI64::new(0);
static RECENT_CORRECT:AtomicI64=AtomicI64::new(0);
const ALPHA:f32=0.12;
const BETA:f32=1.0;
const L1:f32=0.0001;
const L2:f32=1.0;

pub struct OnlineModel{
    z:Vec<f32>,
    n:Vec<f32>,
    positives:i64,
    negatives:i64,
    holdout:i64,
    correct:i64,
    recent_window:u32,
    recent_count:u8,
}
fn digest(s:&str)->String{format!("{:x}",Sha256::digest(s.as_bytes()))}
fn hash(s:&str)->usize{
    // Deterministic 64-bit FNV-1a. The hash output, not raw text, is stored.
    let mut h=0xcbf29ce484222325u64;
    for &byte in s.as_bytes(){
        h^=u64::from(byte);
        h=h.wrapping_mul(0x100000001b3);
    }
    (h as usize)&(DIM-1)
}
fn identity_group(title:&str)->String{
    let mut group=Vec::new();
    for word in title.split(|c:char|!c.is_ascii_alphanumeric())
        .filter(|s|!s.is_empty()).take(12){
        let lower=word.to_ascii_lowercase();
        let season_episode=lower.starts_with('s')&&lower[1..].bytes()
            .next().is_some_and(|c|c.is_ascii_digit());
        let standalone_episode=lower.starts_with('e')&&lower[1..].bytes()
            .next().is_some_and(|c|c.is_ascii_digit());
        let release_year=lower.len()==4 && lower.parse::<i32>()
            .is_ok_and(|y|(1900..=2100).contains(&y));
        if ["2160p","1080p","720p","4320p","4k","8k","x264","x265",
            "hevc","av1","webrip","webdl","bluray","hdtv","aac","ddp",
            "ac3","mkv","mp4"].contains(&lower.as_str())
            ||(!group.is_empty() && (season_episode||standalone_episode||release_year)){
            break;
        }
        group.push(lower);
    }
    group.into_iter().take(8).collect::<Vec<_>>().join(" ")
}
fn features(field:&str,signal:&str,title:&str)->Vec<usize>{
    // Include no film-specific names: only structural features, field/tag
    // n-grams, and position/technical context. Never persist raw filenames.
    let mut set=BTreeSet::new();
    set.insert(hash("bias"));
    set.insert(hash(&format!("f:{field}")));
    set.insert(hash(&format!("s:{signal}")));
    set.insert(hash(&format!("fs:{field}:{signal}")));
    let chars=signal.as_bytes();
    for span in [2,3,4]{
        for chunk in chars.windows(span).take(32){
            if let Ok(token)=std::str::from_utf8(chunk){
                set.insert(hash(&format!("g{span}:{field}:{token}")));
            }
        }
    }
    let lower=title.to_ascii_lowercase();
    let marker=signal.replace('_',".");
    let index=lower.find(signal).or_else(||lower.find(&marker));
    if let Some(index)=index{
        let bucket=index.saturating_mul(8)/lower.len().max(1);
        set.insert(hash(&format!("position:{field}:{}",bucket.min(7))));
    }
    for (label,markers) in [
        ("video", &["1080p","2160p","720p","4k","8k"][..]),
        ("release",&["web","bluray","hdtv","bdrip"][..]),
        ("audio",&["aac","ac3","ddp","dts","opus"][..]),
    ]{
        if markers.iter().any(|m|lower.contains(m)){
            set.insert(hash(&format!("context:{field}:{label}")));
        }
    }
    set.into_iter().collect()
}
impl OnlineModel{
    pub fn new()->Self{
        Self{z:vec![0.0;DIM],n:vec![0.0;DIM],positives:0,negatives:0,
            holdout:0,correct:0,recent_window:0,recent_count:0}
    }
    fn weight(&self,i:usize)->f32{
        let z=self.z[i];
        if z.abs()<=L1{return 0.0;}
        -(z-z.signum()*L1)/((BETA+self.n[i].sqrt())/ALPHA+L2)
    }
    fn raw_score(&self,idx:&[usize])->f32{
        let logit=idx.iter().map(|&i|self.weight(i)).sum::<f32>().clamp(-30.0,30.0);
        1.0/(1.0+(-logit).exp())
    }
    fn learn(&mut self,idx:&[usize],positive:bool){
        let prediction=self.raw_score(idx);
        let gradient=prediction-f32::from(u8::from(positive));
        for &i in idx{
            let g=gradient;
            let n=self.n[i];
            let sigma=((n+g*g).sqrt()-n.sqrt())/ALPHA;
            let old_weight=self.weight(i);
            self.z[i]+=g-sigma*old_weight;
            self.n[i]=n+g*g;
        }
        if positive{self.positives+=1;}else{self.negatives+=1;}
    }
    pub fn score(&self,field:&str,signal:&str,title:&str)->Option<f32>{
        if self.positives<16||self.negatives<16{return None;}
        Some(self.raw_score(&features(field,signal,title)))
    }
    #[cfg(test)]
    pub fn trained(&self)->(i64,i64){(self.positives,self.negatives)}
    pub fn publish_status(&self){
        POSITIVE.store(self.positives,Ordering::Release);
        NEGATIVE.store(self.negatives,Ordering::Release);
        HOLDOUT.store(self.holdout,Ordering::Release);
        HOLDOUT_CORRECT.store(self.correct,Ordering::Release);
        RECENT_HOLDOUT.store(i64::from(self.recent_count),Ordering::Release);
        RECENT_CORRECT.store(i64::from(self.recent_window.count_ones()),Ordering::Release);
    }
    pub fn status()->Value{
        let positive=POSITIVE.load(Ordering::Acquire);
        let negative=NEGATIVE.load(Ordering::Acquire);
        let holdout=HOLDOUT.load(Ordering::Acquire);
        let correct=HOLDOUT_CORRECT.load(Ordering::Acquire);
        let recent=RECENT_HOLDOUT.load(Ordering::Acquire);
        let recent_good=RECENT_CORRECT.load(Ordering::Acquire);
        json!({"engine":"rust_ftrl_proximal","shadow_only":true,
            "weight_slots":DIM,"parameter_bytes":MAX_BYTES,
            "positive_labels":positive,"counterfactual_negative_labels":negative,
            "holdout_pairs":holdout,
            "holdout_pair_accuracy":if holdout>0{
                Some(correct as f64/holdout as f64)}else{None},
            "recent_holdout_pairs":recent,
            "recent_pair_accuracy":if recent>0{
                Some(recent_good as f64/recent as f64)}else{None},
            "drift_review_suggested":recent>=24 && recent_good*4<recent*3,
            "trained":positive>=16 && negative>=16,
            "can_authorize_rules":false})
    }
    pub fn init(db:&Connection)->Result<(),String>{
        db.execute_batch("CREATE TABLE IF NOT EXISTS online_model(
                epoch INTEGER PRIMARY KEY,parameters BLOB NOT NULL,
                positives INTEGER NOT NULL,negatives INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS online_model_seen(
                sample_hash TEXT PRIMARY KEY,seen_at INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS online_model_holdout(
                epoch INTEGER PRIMARY KEY,holdout INTEGER NOT NULL,
                correct INTEGER NOT NULL,recent_window INTEGER NOT NULL,
                recent_count INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS online_model_build(
                epoch INTEGER PRIMARY KEY,source_build TEXT NOT NULL);")
            .map_err(|e|e.to_string())
    }
    pub fn load(db:&Connection)->Result<Self,String>{
        Self::init(db)?;
        let current=current_build();
        let prior:Option<String>=db.query_row(
            "SELECT source_build FROM online_model_build WHERE epoch=?1",
            [EPOCH],|row|row.get(0)
        ).optional().map_err(|e|e.to_string())?;
        if prior.as_deref()!=Some(current.as_str()){
            // A new parser/image build invalidates trained weights and
            // deduplication provenance; historical raw evidence remains in
            // its separate evidence tables for independent revalidation.
            db.execute("DELETE FROM online_model_seen",[])
                .map_err(|e|e.to_string())?;
            db.execute("DELETE FROM online_model",[])
                .map_err(|e|e.to_string())?;
            db.execute("DELETE FROM online_model_holdout",[])
                .map_err(|e|e.to_string())?;
            db.execute("INSERT INTO online_model_build(epoch,source_build)
                VALUES(?1,?2) ON CONFLICT(epoch) DO UPDATE SET
                source_build=excluded.source_build",
                params![EPOCH,current]).map_err(|e|e.to_string())?;
        }
        let saved:Option<(Vec<u8>,i64,i64)>=db.query_row(
            "SELECT parameters,positives,negatives FROM online_model WHERE epoch=?1",
            [EPOCH],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?))
        ).optional().map_err(|e|e.to_string())?;
        let Some((data,positives,negatives))=saved else{return Ok(Self::new());};
        if data.len()!=MAX_BYTES||positives<0||negatives<0
            ||positives>MAX_EXAMPLES||negatives>MAX_EXAMPLES*2{
            db.execute("DELETE FROM online_model_seen",[])
                .map_err(|e|e.to_string())?;
            db.execute("DELETE FROM online_model WHERE epoch=?1",[EPOCH])
                .map_err(|e|e.to_string())?;
            return Ok(Self::new());
        }
        let mut model=Self::new();
        for (i,chunk) in data.chunks_exact(4).enumerate(){
            let value=f32::from_le_bytes([chunk[0],chunk[1],chunk[2],chunk[3]]);
            if !value.is_finite()||(value<0.0 && i>=DIM)||value.abs()>1.0e9{
                db.execute("DELETE FROM online_model_seen",[])
                    .map_err(|e|e.to_string())?;
                db.execute("DELETE FROM online_model WHERE epoch=?1",[EPOCH])
                    .map_err(|e|e.to_string())?;
                return Ok(Self::new());
            }
            if i<DIM{model.z[i]=value;}else{model.n[i-DIM]=value;}
        }
        model.positives=positives;
        model.negatives=negatives;
        let quality:Option<(i64,i64,i64,i64)>=db.query_row(
            "SELECT holdout,correct,recent_window,recent_count
             FROM online_model_holdout WHERE epoch=?1",[EPOCH],
            |row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?))
        ).optional().map_err(|e|e.to_string())?;
        if let Some((holdout,correct,window,count))=quality
            && holdout>=0 && (0..=holdout).contains(&correct)
            && (0..=u32::MAX as i64).contains(&window)
            && (0..=32).contains(&count){
            model.holdout=holdout;
            model.correct=correct;
            model.recent_window=window as u32;
            model.recent_count=count as u8;
        }
        Ok(model)
    }
    pub fn save(&self,db:&Connection)->Result<(),String>{
        let mut data=Vec::with_capacity(MAX_BYTES);
        for &v in self.z.iter().chain(self.n.iter()){data.extend_from_slice(&v.to_le_bytes());}
        db.execute("INSERT INTO online_model(epoch,parameters,positives,negatives)
             VALUES(?1,?2,?3,?4) ON CONFLICT(epoch) DO UPDATE SET
             parameters=excluded.parameters,positives=excluded.positives,
             negatives=excluded.negatives",
             params![EPOCH,data,self.positives,self.negatives])
            .map_err(|e|e.to_string())?;
        db.execute("INSERT INTO online_model_holdout(
                epoch,holdout,correct,recent_window,recent_count)
             VALUES(?1,?2,?3,?4,?5)
             ON CONFLICT(epoch) DO UPDATE SET holdout=excluded.holdout,
                 correct=excluded.correct,recent_window=excluded.recent_window,
                 recent_count=excluded.recent_count",
             params![EPOCH,self.holdout,self.correct,i64::from(self.recent_window),
                i64::from(self.recent_count)]).map_err(|e|e.to_string())?;
        Ok(())
    }
    pub fn observe(&mut self,db:&Connection,title:&str,field:&str,
        signal:&str)->Result<bool,String>{
        if self.positives>=MAX_EXAMPLES||title.len()>240
            ||field.len()>40||signal.len()>32{return Ok(false);}
        // The original Rust parser and field validator already established
        // the positive. A mismatched field is a safe counterfactual negative,
        // not an inferred claim about the film or the final match.
        let sample=digest(&format!("{EPOCH}:{field}:{signal}:{title}"));
        let inserted=db.execute(
            "INSERT OR IGNORE INTO online_model_seen(sample_hash,seen_at)
             VALUES(?1,strftime('%s','now'))",[sample])
            .map_err(|e|e.to_string())?;
        if inserted==0{return Ok(false);}
        if self.positives>0&&self.positives%10_000==0{
            let seen:i64=db.query_row("SELECT count(*) FROM online_model_seen",[],
                |row|row.get(0)).map_err(|e|e.to_string())?;
            if seen>crate::learning_cache::training_max_examples(){
                let prune=(crate::learning_cache::training_max_examples()/10)
                    .clamp(1,10_000);
                db.execute("DELETE FROM online_model_seen WHERE sample_hash IN (
                    SELECT sample_hash FROM online_model_seen
                    ORDER BY seen_at,sample_hash LIMIT ?1)",[prune])
                    .map_err(|e|e.to_string())?;
            }
        }
        let real=features(field,signal,title);
        let false_field=if field=="codec"{"resolution"}else{"codec"};
        let negative=features(false_field,signal,title);
        let group=identity_group(title);
        // A deterministic title-group holdout prevents ordinary examples
        // for the same recognized work leaking between train and review.
        // This is an advisory pairwise check, NOT calibrated production
        // precision or independently verified IMDb matching accuracy.
        if !group.is_empty() && hash(&format!("holdout:{group}"))%5==0{
            if self.positives>=16&&self.negatives>=16{
                let passed=self.raw_score(&real)>self.raw_score(&negative);
                self.holdout+=1;
                if passed{self.correct+=1;}
                self.recent_window=(self.recent_window<<1)|u32::from(passed);
                self.recent_count=self.recent_count.saturating_add(1).min(32);
            }
        }else{
            self.learn(&real,true);
            self.learn(&negative,false);
        }
        Ok(true)
    }
}
#[cfg(test)]
mod tests{
    use super::*;
    #[test]
    fn learns_real_examples_and_rejects_counterfactual_fields(){
        let mut model=OnlineModel::new();
        for k in 0..160{
            let title=format!("Film{k}.2160p.WEB-DL");
            let real=features("resolution","2160p",&title);
            let wrong=features("codec","2160p",&title);
            model.learn(&real,true);
            model.learn(&wrong,false);
        }
        let real=model.score("resolution","2160p","New Film.2160p.WEB-DL").unwrap();
        let wrong=model.score("codec","2160p","New Film.2160p.WEB-DL").unwrap();
        assert!(real>wrong,"real {real} wrong {wrong}");
    }
    #[test]
    fn state_is_versioned_bounded_and_roundtrips_without_titles(){
        let db=Connection::open_in_memory().unwrap();
        let mut model=OnlineModel::load(&db).unwrap();
        assert!(model.score("codec","x265","Film.x265").is_none());
        for k in 0..20{
            assert!(model.observe(&db,&format!("Film{k}.x265"),"codec","x265").unwrap());
        }
        assert!(!model.observe(&db,"Film0.x265","codec","x265").unwrap());
        model.save(&db).unwrap();
        let loaded=OnlineModel::load(&db).unwrap();
        assert!(loaded.trained().0>0);
        assert_eq!(loaded.trained(),model.trained());
        assert_eq!(loaded.score("codec","x265","Film.x265"),
            model.score("codec","x265","Film.x265"));
        let size:i64=db.query_row(
            "SELECT length(parameters) FROM online_model WHERE epoch=?1",
            [EPOCH],|row|row.get(0)).unwrap();
        assert_eq!(size,MAX_BYTES as i64);
    }
    #[test]
    fn same_title_with_different_encodes_stays_in_one_holdout_group(){
        assert_eq!(identity_group("The.Movie.1080p.x265.WEB-DL"),
            identity_group("The.Movie.2160p.AV1.BluRay"));
        let db=Connection::open_in_memory().unwrap();
        let mut model=OnlineModel::load(&db).unwrap();
        for k in 0..80{
            model.observe(&db,&format!("Training{k}.2160p.WEB-DL"),
                "resolution","2160p").unwrap();
        }
        let candidate=(0..500).find(|k|{
            let name=format!("Unseen{k}.2160p.WEB-DL");
            hash(&format!("holdout:{}",identity_group(&name)))%5==0
        }).unwrap();
        let title=format!("Unseen{candidate}.2160p.WEB-DL");
        let before=model.positives;
        let evaluation=model.holdout;
        assert!(model.observe(&db,&title,"resolution","2160p").unwrap());
        assert_eq!(model.positives,before,
            "holdout must never update the model weights");
        assert_eq!(model.holdout,evaluation+1);
        model.save(&db).unwrap();
        let restored=OnlineModel::load(&db).unwrap();
        assert_eq!(restored.holdout,model.holdout);
    }
    #[test]
    fn stale_parser_build_invalidates_model_and_training_dedup(){
        let db=Connection::open_in_memory().unwrap();
        let mut model=OnlineModel::load(&db).unwrap();
        for k in 0..40 {
            model.observe(&db,&format!("Film{k}.2160p.WEB-DL"),
                "resolution","2160p").unwrap();
        }
        model.save(&db).unwrap();
        db.execute("UPDATE online_model_build SET source_build='older-build'
            WHERE epoch=?1",[EPOCH]).unwrap();
        let restored=OnlineModel::load(&db).unwrap();
        assert_eq!(restored.trained(),(0,0));
        let rows:i64=db.query_row("SELECT count(*) FROM online_model_seen",
            [],|row|row.get(0)).unwrap();
        assert_eq!(rows,0);
    }
    #[test]
    fn corrupt_weights_fail_closed(){
        let db=Connection::open_in_memory().unwrap();
        OnlineModel::init(&db).unwrap();
        db.execute("INSERT INTO online_model VALUES(?1,x'00',200,200)",[EPOCH]).unwrap();
        assert_eq!(OnlineModel::load(&db).unwrap().trained(),(0,0));
    }
}
