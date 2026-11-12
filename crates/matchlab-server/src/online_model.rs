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
fn hash_bytes(parts:&[&[u8]])->usize{
    // Byte-exact FNV-1a over virtual concatenation: no String allocation
    // for dozens of field/feature n-grams per model prediction.
    let mut h=0xcbf29ce484222325u64;
    for part in parts{
        for &byte in *part{
            h^=u64::from(byte);
            h=h.wrapping_mul(0x100000001b3);
        }
    }
    (h as usize)&(DIM-1)
}
fn hash(s:&str)->usize{hash_bytes(&[s.as_bytes()])}
fn identity_group(title:&str)->String{
    let mut group=Vec::new();
    // The holdout identity uses only the first eight source words. Scanning
    // four additional words and allocating a second Vec never affected the
    // hash, but happened for every observed torrent.
    for word in title.split(|c:char|!c.is_ascii_alphanumeric())
        .filter(|s|!s.is_empty()).take(8){
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
    group.join(" ")
}
// Per-release structural context: shared between up to 64 field observations
// and between all candidate-discovery scores for a title. Never persisted.
pub struct TitleFeatureContext{
    lower:String,
    contexts:[bool;3],
    holdout:bool,
}
impl TitleFeatureContext{
    pub fn new(title:&str)->Self{
        let lower=title.to_ascii_lowercase();
        let contexts=[
            ["1080p","2160p","720p","4k","8k"].iter().any(|m|lower.contains(m)),
            ["web","bluray","hdtv","bdrip"].iter().any(|m|lower.contains(m)),
            ["aac","ac3","ddp","dts","opus"].iter().any(|m|lower.contains(m)),
        ];
        let group=identity_group(title);
        let holdout=!group.is_empty() && hash(&format!("holdout:{group}"))%5==0;
        Self{lower,contexts,holdout}
    }
    fn features(&self,field:&str,signal:&str)->Vec<usize>{
        // Preserve the exact legacy FNV-1a feature IDs and sorted-unique order
        // so pretrained FTRL state and floating-point update order are stable.
        // A short Vec avoids a balanced-tree allocation for each feature.
        let mut features=Vec::with_capacity(110);
        let field_bytes=field.as_bytes();
        let signal_bytes=signal.as_bytes();
        features.push(hash("bias"));
        features.push(hash_bytes(&[b"f:",field_bytes]));
        features.push(hash_bytes(&[b"s:",signal_bytes]));
        features.push(hash_bytes(&[b"fs:",field_bytes,b":",signal_bytes]));
        // Canonical technical markers are overwhelmingly ASCII. When
        // the entire signal is ASCII, every n-gram is valid UTF-8 and
        // individual validation is redundant. Keep identical feature hashes
        // and the exact legacy validation for non-ASCII signals.
        let ascii=signal_bytes.is_ascii();
        for (span,prefix) in [(2,&b"g2:"[..]),(3,&b"g3:"[..]),(4,&b"g4:"[..])]{
            for chunk in signal_bytes.windows(span).take(32){
                if ascii||std::str::from_utf8(chunk).is_ok(){
                    features.push(hash_bytes(&[prefix,field_bytes,b":",chunk]));
                }
            }
        }
        let index=self.lower.find(signal).or_else(||{
            signal.contains('_').then(||signal.replace('_',"."))
                .and_then(|marker|self.lower.find(&marker))
        });
        if let Some(index)=index{
            let bucket=index.saturating_mul(8)/self.lower.len().max(1);
            let position=[b'0'+bucket.min(7) as u8];
            features.push(hash_bytes(&[b"position:",field_bytes,b":",&position]));
        }
        for (present,label) in self.contexts.into_iter().zip(["video","release","audio"]){
            if present{
                features.push(hash_bytes(&[b"context:",field_bytes,b":",label.as_bytes()]));
            }
        }
        features.sort_unstable();
        features.dedup();
        features
    }
}
#[cfg(test)]
fn features(field:&str,signal:&str,title:&str)->Vec<usize>{
    TitleFeatureContext::new(title).features(field,signal)
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
        self.score_prepared(field,signal,&TitleFeatureContext::new(title))
    }
    pub fn score_prepared(&self,field:&str,signal:&str,
        context:&TitleFeatureContext)->Option<f32>{
        if self.positives<16||self.negatives<16{return None;}
        Some(self.raw_score(&context.features(field,signal)))
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
    /// Shadow-train across *all* ordinary parser fields, not only codec,
    /// resolution and anime markers. Labels are replayed from the unmodified
    /// canonical parser. They are advisory, never rule-promotion truth.
    /// A fixed per-release cap bounds SQLite writes and CPU use.
    pub fn observe_canonical_fields(
        &mut self,db:&Connection,title:&str,canonical:&Value
    )->Result<usize,String>{
        let Some(fields)=canonical.as_object()else{return Ok(0);};
        let mut examples=Vec::new();
        for (field,value) in fields {
            if field.starts_with("learned_")
                ||field=="path_title_candidates"
                ||field.len()>40 || value.is_null(){continue;}
            let signal=match value {
                Value::Bool(true)=>"present".to_owned(),
                Value::Number(n)=>{
                    if field=="year" && n.as_i64()
                        .is_some_and(|y|(1880..=2100).contains(&y)){
                        "four_digit_year".to_owned()
                    }else{"number".to_owned()}
                }
                Value::String(v) if !v.trim().is_empty()=>{
                    if field=="title" {
                        // Never turn a particular IMDb work name into a
                        // reusable field token or a source for regex rules.
                        "title_boundary".to_owned()
                    }else{
                        v.chars().filter(char::is_ascii_alphanumeric)
                            .flat_map(char::to_lowercase).take(24).collect()
                    }
                }
                Value::Array(values) if !values.is_empty()=>{
                    if field=="seasons"{"season_coordinates".to_owned()}
                    else if field=="episodes"{"episode_coordinates".to_owned()}
                    else if field=="languages"{"language_list".to_owned()}
                    else if values.iter().all(Value::is_string) {
                        "string_list".to_owned()
                    }else{"coordinates_or_list".to_owned()}
                }
                _=>continue,
            };
            if !signal.is_empty() && signal.len()<=32 {
                examples.push((field.as_str(),signal));
            }
        }
        // Train every nonempty canonical parser field; earlier versions
        // sampled only six fields per title and could systematically miss
        // rarer fields (group, edition, region, site, etc.). The parser
        // produces a finite schema; this cap bounds work from malformed
        // or future oversized payloads without favoring alphabetic keys.
        if examples.is_empty(){return Ok(0);}
        let rotation=hash(title)%examples.len();
        examples.rotate_left(rotation);
        let context=TitleFeatureContext::new(title);
        let mut seen=0;
        for (field,signal) in examples.into_iter().take(64){
            if self.observe_prepared(db,title,field,&signal,&context)?{seen+=1;}
        }
        Ok(seen)
    }
    pub fn observe(&mut self,db:&Connection,title:&str,field:&str,
        signal:&str)->Result<bool,String>{
        if self.positives>=MAX_EXAMPLES||title.len()>240
            ||field.len()>40||signal.len()>32{return Ok(false);}
        self.observe_prepared(db,title,field,signal,&TitleFeatureContext::new(title))
    }
    fn observe_prepared(&mut self,db:&Connection,title:&str,field:&str,
        signal:&str,context:&TitleFeatureContext)->Result<bool,String>{
        if self.positives>=MAX_EXAMPLES||title.len()>240
            ||field.len()>40||signal.len()>32{return Ok(false);}
        // The original Rust parser and field validator already established
        // the positive. A mismatched field is a safe counterfactual negative,
        // not an inferred claim about the film or the final match.
        let sample=digest(&format!("{EPOCH}:{field}:{signal}:{title}"));
        let inserted=db.prepare_cached(
            "INSERT OR IGNORE INTO online_model_seen(sample_hash,seen_at)
             VALUES(?1,strftime('%s','now'))")
            .map_err(|e|e.to_string())?.execute([sample])
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
        let real=context.features(field,signal);
        let false_field=if field=="codec"{"resolution"}else{"codec"};
        let negative=context.features(false_field,signal);
        // A deterministic title-group holdout prevents ordinary examples
        // for the same recognized work leaking between train and review.
        // This is an advisory pairwise check, NOT calibrated production
        // precision or independently verified IMDb matching accuracy.
        if context.holdout{
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
    fn prepared_features_preserve_original_ftrl_indices_and_scoring(){
        use std::collections::BTreeSet;
        for title in ["Film.2160p.WEB-DL.x265", "Jojo's Bizarre Adventure - 27",
            "Série.2025.S02E01.1080p.AAC"]{
            let context=TitleFeatureContext::new(title);
            let lower=title.to_ascii_lowercase();
            for (field,signal) in [("codec","x265"),("resolution","2160p"),
                ("episodes","episode_coordinates"),("quality","WEB-DL"),
                ("year","four_digit_year"),("audio","aac"),
                ("group","part_2"),("subbed","café"),("audio","日本語")]{
                let mut legacy=BTreeSet::new();
                legacy.insert(hash("bias"));
                legacy.insert(hash(&format!("f:{field}")));
                legacy.insert(hash(&format!("s:{signal}")));
                legacy.insert(hash(&format!("fs:{field}:{signal}")));
                for span in [2,3,4]{
                    for chunk in signal.as_bytes().windows(span).take(32){
                        if let Ok(token)=std::str::from_utf8(chunk){
                            legacy.insert(hash(&format!("g{span}:{field}:{token}")));
                        }
                    }
                }
                let marker=signal.replace('_',".");
                if let Some(index)=lower.find(signal).or_else(||lower.find(&marker)){
                    let bucket=index.saturating_mul(8)/lower.len().max(1);
                    legacy.insert(hash(&format!("position:{field}:{}",bucket.min(7))));
                }
                for (label,markers) in [
                    ("video",&["1080p","2160p","720p","4k","8k"][..]),
                    ("release",&["web","bluray","hdtv","bdrip"][..]),
                    ("audio",&["aac","ac3","ddp","dts","opus"][..]),
                ]{
                    if markers.iter().any(|m|lower.contains(m)){
                        legacy.insert(hash(&format!("context:{field}:{label}")));
                    }
                }
                assert_eq!(context.features(field,signal),
                    legacy.into_iter().collect::<Vec<_>>());
            }
        }
    }
    #[test]
    fn shadow_model_observes_all_field_categories_with_bounded_work(){
        let db=Connection::open_in_memory().unwrap();
        let mut learner=OnlineModel::load(&db).unwrap();
        let parsed=json!({"title":"Example Work","year":2024,
            "seasons":[2],"episodes":[7],"resolution":"1080p",
            "quality":"WEB-DL","codec":"hevc","audio":["AAC"],
            "edition":"Director Cut","network":"Apple TV",
            "subbed":true,"group":"EXAMPLE"});
        let n=learner.observe_canonical_fields(
            &db,"Example.Work.S02E07.2024.1080p.WEB-DL",&parsed).unwrap();
        assert_eq!(n,12,"all canonical fields should train instead of sampling six");
        let again=learner.observe_canonical_fields(
            &db,"Example.Work.S02E07.2024.1080p.WEB-DL",&parsed).unwrap();
        assert_eq!(again,0,"repeated queries must not retrain the same title");
        let stored:i64=db.query_row("SELECT COUNT(*) FROM online_model_seen",[],
            |r|r.get(0)).unwrap();
        assert_eq!(stored,12);
    }
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
