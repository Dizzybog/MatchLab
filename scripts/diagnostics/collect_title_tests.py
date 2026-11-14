#!/usr/bin/env python3
"""Conservative extraction of explicit Rust parser-test expectations.

This is deliberately a restricted Rust reader, never an evaluator of arbitrary
source. Unsupported assertions are recorded for manual review. Observed parser
outputs are never used to derive labels. Runtime tracing supplies the complete
executed-input inventory separately.
"""
import argparse
import collections
import dataclasses
import difflib
import hashlib
import json
import pathlib
import re

UNKNOWN = object()
TOK = re.compile(r'//[^\n]*|/\*[\s\S]*?\*/|r(?P<hash>\#*)"[\s\S]*?"(?P=hash)|"(?:\\[\s\S]|[^"\\])*"|\'(?:\\.|[^\'\\])\'|[A-Za-z_][A-Za-z_0-9]*|[0-9][0-9_]*|::|->|=>|==|!=|&&|\|\||\.\.=|\.\.|[^\s]')

@dataclasses.dataclass
class Token:
    text: str
    pos: int

@dataclasses.dataclass
class Input:
    title: str
    method: str = 'parse_title'
    translate: bool = False

@dataclasses.dataclass
class Field:
    input: Input
    path: tuple = ()
    coercion: str = ''


def tokens(source):
    return [Token(m.group(),m.start()) for m in TOK.finditer(source) if not m.group().startswith(('//','/*'))]

def split(ts, sep=','):
    out=[]; start=0; depth=0
    for i,t in enumerate(ts):
        if t.text in ('(','[','{'): depth+=1
        elif t.text in (')',']','}'): depth-=1
        elif t.text==sep and depth==0:
            out.append(ts[start:i]); start=i+1
    if start<len(ts): out.append(ts[start:])
    return out

def close(ts,start):
    opening=ts[start].text; closing={'(':')','[':']','{':'}'}[opening]; depth=0
    for i in range(start,len(ts)):
        if ts[i].text==opening: depth+=1
        elif ts[i].text==closing:
            depth-=1
            if depth==0:return i
    return len(ts)-1

def text(ts):return ''.join(t.text for t in ts)

def rust_string(raw):
    if raw.startswith('r'):
        return raw[raw.index('"')+1:raw.rindex('"')]
    # JSON covers common Rust escapes; translate Rust-only scalar syntax.
    raw=re.sub(r'\\u\{([0-9a-fA-F_]+)\}',lambda m:json.dumps(chr(int(m[1].replace('_',''),16)))[1:-1],raw)
    raw=re.sub(r'\\x([0-9a-fA-F]{2})',lambda m:json.dumps(chr(int(m[1],16)))[1:-1],raw)
    return json.loads(raw.replace('\\0','\\u0000'))

class Reader:
    def __init__(self,root):
        self.root=root; self.rows={}; self.inventory=[]; self.failures=[]; self.context=None
    def row(self,inp):
        key=(inp.title,inp.translate)
        if key not in self.rows:self.rows[key]={'title':inp.title,'translate_languages':inp.translate,'expected':[],'sources':[],'assertions_manual':[]}
        row=self.rows[key]
        if self.context not in row['sources']:row['sources'].append(self.context.copy())
        return row
    def expectation(self,field,op,value=UNKNOWN):
        if not isinstance(field,Field) or not field.path:return
        row=self.row(field.input)
        item={'path':list(field.path),'op':op}
        if value is not UNKNOWN:item['value']=value
        if field.coercion:item['coercion']=field.coercion
        if item not in row['expected']:row['expected'].append(item)
    def evaluate(self,ts,env):
        ts=list(ts)
        while ts and ts[0].text=='&':ts=ts[1:]
        if not ts:return UNKNOWN
        if ts[0].text=='if':
            try:
                a=next(i for i,t in enumerate(ts) if t.text=='{'); ae=close(ts,a)
                condition=self.evaluate(ts[1:a],env)
                if condition is UNKNOWN:return UNKNOWN
                if condition:return self.evaluate(ts[a+1:ae],env)
                b=ae+2
                return self.evaluate(ts[b+1:close(ts,b)],env)
            except (StopIteration,KeyError,IndexError):return UNKNOWN
        # Arithmetic is used only for integer range counts in test labels.
        for op in ('+','-'):
            parts=split(ts,op)
            if len(parts)==2:
                a,b=[self.evaluate(p,env) for p in parts]
                if isinstance(a,int) and isinstance(b,int):return a+b if op=='+' else a-b
        first=ts[0].text
        consumed=1
        try:
            if first.startswith(('"','r"','r#')):value=rust_string(first)
            elif first.startswith("'") and first.endswith("'"):value=first[1:-1]
            elif first.isdigit() or re.fullmatch(r'[0-9][0-9_]*',first):value=int(first.replace('_',''))
            elif first in ('true','false'):value=first=='true'
            elif first in ('None','null'):value=None
            elif first in ('(','['):
                end=close(ts,0); vals=[self.evaluate(p,env) for p in split(ts[1:end])]
                value=vals[0] if first=='(' and len(vals)==1 else vals
                consumed=end+1
            elif first in ('json','vec','format') and len(ts)>2 and ts[1].text=='!':
                end=close(ts,2); args=split(ts[3:end]); consumed=end+1
                vals=[self.evaluate(p,env) for p in args]
                if first=='json':value=vals[0] if vals else UNKNOWN
                elif first=='vec':value=vals
                elif vals and isinstance(vals[0],str):
                    fmt=vals[0]; rest=iter(vals[1:]); unresolved=[]
                    def repl(m):
                        key=m[1]; val=env.get(key,UNKNOWN) if key else next(rest,UNKNOWN)
                        
                        if val is UNKNOWN:unresolved.append(key);return m[0]
                        return str(val)
                    value=re.sub(r'\{([A-Za-z_0-9]*)\}',repl,fmt)
                    if unresolved:value=UNKNOWN
                else:value=UNKNOWN
            elif first=='variants' and len(ts)>1 and ts[1].text=='(':
                end=close(ts,1);args=[self.evaluate(p,env) for p in split(ts[2:end])];consumed=end+1
                if len(args)==5 and all(v is not UNKNOWN for v in args):
                    title,year,season,episode,series=args
                    dot=title.replace(' ','.');dash=title.replace(' ','-')
                    suffixes=['1080p.WEB-DL.x264-GROUP','2160p.BluRay.REMUX.HEVC','720p.WEBRip.AAC','1080p.NF.WEB-DL.DDP5.1','2160p.UHD.HDR.DV','480p.DVDRip.XviD','1080p.BluRay.x265.10bit','720p.HDTV.x264','1080p.AMZN.WEB-DL.H264','2160p.WEB.H265.Atmos']
                    value=[]
                    for index,suffix in enumerate(suffixes):
                        prefix=dot if index%2==0 else dash
                        if series:
                            value.append([f'{prefix}.S{season:02}E{episode:02}.{suffix}',f'{prefix} S{season:02}E{episode:02} {suffix}',f'{prefix}.S{season:02}.E{episode:02}.{suffix}',f'{prefix} - S{season:02}E{episode:02} - {suffix}'][index%4])
                        else:
                            value.append([f'{prefix}.{year}.{suffix}',f'{prefix} ({year}) {suffix}',f'{prefix}-{year}-{suffix}'][index%3])
                else:value=UNKNOWN
            elif first=='Some' and len(ts)>1 and ts[1].text=='(':
                end=close(ts,1);value=self.evaluate(ts[2:end],env);consumed=end+1
            elif first in ('parse','parsed','parse_title','parse_title_canonical') and len(ts)>1 and ts[1].text=='(':
                end=close(ts,1);args=[self.evaluate(p,env) for p in split(ts[2:end])];consumed=end+1
                if args and isinstance(args[0],str):
                    inp=Input(args[0],first,bool(args[1]) if len(args)>1 and args[1] is not UNKNOWN else False)
                    self.row(inp);value=Field(inp)
                else:value=UNKNOWN
            else:value=env.get(first,UNKNOWN)
            # Field paths, simple pure methods, and parser result wrappers.
            while consumed<len(ts):
                t=ts[consumed].text
                if t=='[':
                    end=close(ts,consumed);idx=self.evaluate(ts[consumed+1:end],env)
                    if isinstance(value,Field) and idx is not UNKNOWN:value=Field(value.input,value.path+(idx,))
                    elif isinstance(value,(list,tuple)) and isinstance(idx,int):value=value[idx]
                    else:value=UNKNOWN
                    consumed=end+1
                elif t=='.' and consumed+1<len(ts):
                    method=ts[consumed+1].text;consumed+=2
                    args=[]
                    if consumed<len(ts) and ts[consumed].text=='(':
                        end=close(ts,consumed);args=[self.evaluate(p,env) for p in split(ts[consumed+1:end])];consumed=end+1
                    if method in ('unwrap','expect','unwrap_or_else','to_owned','to_string','into','iter','copied','as_ref'):pass
                    elif method=='get' and isinstance(value,Field) and args and args[0] is not UNKNOWN:value=Field(value.input,value.path+(args[0],))
                    elif method in ('as_i64','as_u64','as_str','as_bool','as_array') and isinstance(value,Field):pass
                    elif method=='and_then' and isinstance(value,Field) and any(t.text in ('as_str','as_i64','as_u64','as_bool','as_array') for t in ts):pass
                    elif method=='len' and isinstance(value,Field):value=Field(value.input,value.path,'len')
                    elif method=='contains' and isinstance(value,str) and args and isinstance(args[0],str):value=args[0] in value
                    elif method=='to_ascii_lowercase' and isinstance(value,str):value=value.lower()
                    elif method in ('clone','as_slice'):pass
                    else:value=UNKNOWN
                else:return UNKNOWN
            return value
        except (ValueError,IndexError,TypeError,StopIteration,json.JSONDecodeError):return UNKNOWN
    def assertion(self,name,args,env):
        if not args:return
        a=self.evaluate(args[0],env)
        if name in ('assert_eq','assert_ne') and len(args)>1:
            b=self.evaluate(args[1],env)
            if isinstance(a,Field) and b is not UNKNOWN and not isinstance(b,(Field,Input)):
                self.expectation(a,'eq' if name=='assert_eq' else 'ne',b);return
            if isinstance(b,Field) and a is not UNKNOWN and not isinstance(a,(Field,Input)):
                self.expectation(b,'eq' if name=='assert_eq' else 'ne',a);return
        condition=text(args[0])
        # Only simple absence predicates are scored automatically.
        m=re.fullmatch(r'(.+)\.is_none\(\)',condition)
        if name=='assert' and m:
            prefix=tokens(m[1]);v=self.evaluate(prefix,env)
            if isinstance(v,Field):self.expectation(v,'absent');return
        # Preserve source assertions even when not executable by this reader.
        names={t.text for t in args[0] if re.fullmatch(r'[A-Za-z_]\w*',t.text)}
        fields=[env[n] for n in names if isinstance(env.get(n),Field)]
        for f in fields:
            row=self.row(f.input)
            if condition not in row['assertions_manual']:row['assertions_manual'].append(condition)
    def block(self,ts,env):
        i=0
        while i<len(ts):
            t=ts[i].text
            if t=='let':
                end=i+1;depth=0;equal=None
                while end<len(ts):
                    c=ts[end].text
                    if c in ('(','[','{'):depth+=1
                    elif c in (')',']','}'):depth-=1
                    if c=='=' and depth==0 and equal is None:equal=end
                    if c==';' and depth==0:break
                    end+=1
                if equal is not None:
                    names=[x.text for x in ts[i+1:equal] if re.fullmatch(r'[A-Za-z_]\w*',x.text) and x.text not in ('mut','ref')]
                    if names:env[names[0]]=self.evaluate(ts[equal+1:end],env)
                i=end+1;continue
            if t=='for':
                j=i+1
                while j<len(ts) and ts[j].text!='in':j+=1
                body=j+1;depth=0
                while body<len(ts):
                    c=ts[body].text
                    if c=='{' and depth==0:break
                    if c in ('(','['):depth+=1
                    elif c in (')',']'):depth-=1
                    body+=1
                if body>=len(ts):break
                end=close(ts,body);values=self.evaluate(ts[j+1:body],env)
                names=[x.text for x in ts[i+1:j] if re.fullmatch(r'[A-Za-z_]\w*',x.text)]
                if isinstance(values,list):
                    for value in values:
                        local=env.copy();bindings=value if len(names)>1 and isinstance(value,list) else [value]
                        local.update(zip(names,bindings));self.block(ts[body+1:end],local)
                else:self.failures.append({**self.context,'unexpanded_loop':text(ts[i:body])})
                i=end+1;continue
            if t in ('assert_eq','assert_ne','assert') and i+2<len(ts) and ts[i+1].text=='!':
                end=close(ts,i+2);self.assertion(t,split(ts[i+3:end]),env);i=end+1;continue
            if t in ('if','match','while','loop'):
                # Conditional/unsupported control flow cannot establish labels.
                while i<len(ts) and ts[i].text!='{':i+=1
                if i<len(ts):i=close(ts,i)+1
                if i<len(ts) and ts[i].text=='else':
                    i+=1
                    if i<len(ts) and ts[i].text=='{':i=close(ts,i)+1
                continue
            i+=1
    def file(self,path):
        source=path.read_text();ts=tokens(source);globals={}
        for i,t in enumerate(ts):
            if t.text=='const' and i+1<len(ts):
                j=i+2
                while j<len(ts) and ts[j].text not in ('=',';'):j+=1
                if j<len(ts) and ts[j].text=='=':
                    k=j+1;depth=0
                    while k<len(ts):
                        if ts[k].text in ('[','(','{'):depth+=1
                        elif ts[k].text in (']',')','}'):depth-=1
                        if ts[k].text==';' and depth==0:break
                        k+=1
                    globals[ts[i+1].text]=self.evaluate(ts[j+1:k],globals)
        for i,t in enumerate(ts):
            if t.text=='#' and i+3<len(ts) and text(ts[i:i+4])=='#[test]':
                j=i+4
                while j<len(ts) and ts[j].text!='fn':j+=1
                if j+1>=len(ts):continue
                name=ts[j+1].text
                while j<len(ts) and ts[j].text!='{':j+=1
                end=close(ts,j)
                self.context={'file':str(path.relative_to(self.root)),'test':name,'line':source.count('\n',0,t.pos)+1}
                self.inventory.append(self.context.copy())
                self.block(ts[j+1:end],globals.copy())
    def run(self):
        for path in sorted((self.root/'crates/matchlab-parser/tests').glob('*.rs')):self.file(path)
        self.file(self.root/'crates/matchlab-parser/src/lib.rs')
        self.file(self.root/'crates/matchlab-parser/src/prefilter.rs')
        return self.payload()
    def payload(self):
        rows=list(self.rows.values())
        # Unsupported intermediate expressions are never emitted as JSON labels.
        def safe(v):
            if v is UNKNOWN or isinstance(v,(Field,Input)):return False
            if isinstance(v,list):return all(safe(x) for x in v)
            if isinstance(v,dict):return all(safe(x) for x in v.values())
            return True
        for row in rows:row['expected']=[e for e in row['expected'] if safe(e)]
        return {'schema':'matchlab.parser-test-expectations.v1','provenance':'Explicit source assertions only; parser outputs are not ground truth. Restricted extraction is incomplete; runtime trace is the executed-input inventory.','tests':self.inventory,'unexpanded_loops':self.failures,'cases':rows,'summary':{'test_functions':len(self.inventory),'unique_inputs':len(rows),'inputs_with_scored_expectations':sum(bool(r['expected']) for r in rows),'scored_assertions':sum(len(r['expected']) for r in rows),'manual_assertions':sum(len(r['assertions_manual']) for r in rows),'unexpanded_loops':len(self.failures)}}

# The small variants() mirror above is audited against this exact tokenized
# source function. Fail closed after semantic changes instead of silently
# manufacturing incorrect test labels. Comments/whitespace do not affect it.
VARIANT_SOURCE_SHA256 = "c8951c84c7afbe963a86fa8ac63b5fcf102cdae72074c722feba8ec6e0719cb8"

def verify_variant_source(path):
    ts=tokens(path.read_text())
    for i,t in enumerate(ts[:-1]):
        if t.text=="fn" and ts[i+1].text=="variants":
            start=next(j for j in range(i,len(ts)) if ts[j].text=="{")
            end=close(ts,start)
            digest=hashlib.sha256(text(ts[i:end+1]).encode()).hexdigest()
            if digest==VARIANT_SOURCE_SHA256:return
            raise ValueError("release_300::variants changed; audit the collector mirror before using its labels")
    raise ValueError("release_300::variants function was not found")


def collect(root):
    release_path=root/'crates/matchlab-server/tests/release_300.rs'
    verify_variant_source(release_path)
    parser=Reader(root).run()
    release_reader=Reader(root);release_reader.file(release_path)
    release=release_reader.payload()
    release['provenance']='Explicit release_300.rs source assertions only; audited generator mirror, never output-derived labels.'
    inventory={'provenance':'Static test-function inventory; runtime trace is required to inventory every executed dynamic input.',
        'tests':parser['tests']+release['tests'],
        'summary':{'test_functions':len(parser['tests'])+len(release['tests']),
            'parser_cases':parser['summary']['unique_inputs'],
            'release_300_cases':release['summary']['unique_inputs'],
            'source_assertions':parser['summary']['scored_assertions']+release['summary']['scored_assertions'],
            'manual_unscored':parser['summary']['manual_assertions']+release['summary']['manual_assertions']},
        'unexpanded_loops':parser['unexpanded_loops']+release['unexpanded_loops']}
    return parser,release,inventory


def summarize_trace(trace,curated_titles):
    """Keep runtime observations separate from source expectation labels."""
    counts=collections.Counter();inputs={};errors=[];empty_revision=0xcbf29ce484222325
    for number,line in enumerate(trace.read_text().splitlines(),1):
        try:row=json.loads(line)
        except json.JSONDecodeError as error:
            errors.append({'line':number,'error':str(error)});continue
        counts['records']+=1;counts['kind:'+row['kind']]+=1;counts['mode:'+row['parser_mode']]+=1
        key=(row['title'],row['translate_languages'])
        item=inputs.setdefault(key,{'title':row['title'],'translate_languages':row['translate_languages'],
            'stages':set(),'threads':set(),'callers':set(),'public_observations':{},'variants':set(),'occurrences':0})
        item['occurrences']+=1;item['stages'].add(row['kind']);item['threads'].add(row['thread'] or '<unnamed-worker>')
        item['callers'].add((row['caller_file'],row['caller_line']))
        if row.get('observed') is not None:
            serialized=json.dumps(row['observed'],sort_keys=True,separators=(',',':'),ensure_ascii=False)
            item['variants'].add(hashlib.sha256(serialized.encode()).hexdigest())
            baseline=(row['parser_mode']=='canonical' or row['parser_mode']=='live' and row['revision'] in (0,empty_revision))
            if row['kind']=='parse_title' and baseline:item['public_observations'][serialized]=row['observed']
        if row.get('error'):counts['parse_errors']+=1
    inventory=[];parity=[]
    for _,item in sorted(inputs.items()):
        observations=item.pop('public_observations');item['stages']=sorted(item['stages']);item['threads']=sorted(item['threads'])
        item['callers']=[{'file':f,'line':n} for f,n in sorted(item['callers'])]
        item['all_observation_variants']=len(item.pop('variants'));item['public_baseline_variants']=len(observations)
        inventory.append(item)
        if len(observations)==1 and not item['translate_languages'] and 0<len(item['title'].encode())<=4096:
            title=item['title'];observed=next(iter(observations.values()))
            parity.append({'id':'trace-'+hashlib.sha256(title.encode()).hexdigest()[:16],
                'endpoint':'/parse','request':{'title':title},'classification':'local_remote_differential',
                'expected':{},'local_observed':observed,'assertions':[{'path':[],'op':'eq','value':observed}],
                'expected_provenance':'LOCAL OBSERVATION ONLY; version parity, never independent correctness.',
                'sources':item['callers'],'test_threads':item['threads']})
    summary={'counts':dict(counts),'unique_inputs_including_engine_components':len(inventory),
        'unique_public_inputs':sum('parse_title' in i['stages'] for i in inventory),
        'unique_batch_inputs':sum('parse_many_input' in i['stages'] for i in inventory),
        'unique_engine_only_inputs':sum(i['stages']==['engine'] for i in inventory),
        'stable_public_false_translation_parity_cases':len(parity),
        'unstable_public_baseline_inputs':sum(i['public_baseline_variants']>1 for i in inventory),
        'invalid_trace_lines':errors}
    return {
        'executed-title-inventory.json':{'provenance':'Executed inputs/observations, never independent expected labels.',
            'summary':summary,'inputs':inventory},
        'title-parity-corpus.json':{'provenance':'Stable local observations for version comparison only.','cases':parity},
        'title-parity-new-only.json':{'provenance':'Stable local observations absent from parser golden corpus; comparison only.',
            'cases':[r for r in parity if r['request']['title'] not in curated_titles]},
        'trace-summary.json':summary}


def trace_patch(root):
    """Emit reviewable instrumentation; never modify the selected checkout."""
    relative='crates/matchlab-parser/src/lib.rs'
    original=(root/relative).read_text();changed=original
    if 'fn diagnostic_title_trace(' in original:
        raise ValueError('Selected source is already instrumented')
    signature="""pub fn parse_title(
    raw_title: &str,
    translate_languages: bool,
) -> Result<Map<String, Value>, ParseError> {"""
    wrapper="""#[track_caller]
"""+signature+"""
    let output=parse_title_untraced(raw_title,translate_languages);
    diagnostic_title_trace("parse_title",raw_title,translate_languages,
        output.as_ref().ok(),output.as_ref().err());
    output
}
"""+signature.replace('pub fn parse_title(', 'fn parse_title_untraced(')
    replacements=[
        (signature,wrapper),
        ('        result.insert("title".to_owned(), Value::String(clean_title(title_slice)));',
         '        result.insert("title".to_owned(), Value::String(clean_title(title_slice)));\n'
         '        diagnostic_title_trace("engine",raw_title,translate_languages,Some(&result),None);'),
        ("pub fn parse_many<'a, I>(", "#[track_caller]\npub fn parse_many<'a, I>("),
        ('    let titles: Vec<_> = titles.into_iter().collect();',
         '    let titles: Vec<_> = titles.into_iter().collect();\n'
         '    for title in &titles {\n'
         '        diagnostic_title_trace("parse_many_input",title,translate_languages,None,None);\n'
         '    }'),
    ]
    for old,new in replacements:
        if changed.count(old)!=1:
            raise ValueError('Trace anchor changed; audit the temporary instrumentation before applying')
        changed=changed.replace(old,new)
    changed+=TRACE_HELPER
    return ''.join(difflib.unified_diff(original.splitlines(keepends=True),
        changed.splitlines(keepends=True),fromfile='a/'+relative,tofile='b/'+relative))


TRACE_HELPER = r"""
#[track_caller]
fn diagnostic_title_trace(
    kind:&str, raw_title:&str, translate_languages:bool,
    output:Option<&Map<String,Value>>, error:Option<&ParseError>,
){
    use std::io::Write;
    let Some(path)=std::env::var_os("MATCHLAB_DIAGNOSTIC_TITLE_TRACE") else{return;};
    static TRACE_LOCK:Mutex<()>=Mutex::new(());
    let Ok(_lock)=TRACE_LOCK.lock() else{return;};
    let thread=std::thread::current();
    let caller=std::panic::Location::caller();
    let mode=if CANONICAL_ONLY.with(Cell::get){"canonical"}
        else if REPLAY_ENGINE.with(|slot|slot.borrow().is_some()){"replay"}
        else {"live"};
    let row=json!({"kind":kind,"title":raw_title,
        "translate_languages":translate_languages,"thread":thread.name(),
        "pid":std::process::id(),"caller_file":caller.file(),
        "caller_line":caller.line(),"parser_mode":mode,
        "revision":handler_patch_revision(),"observed":output,
        "error":error.map(ToString::to_string)});
    let Ok(mut bytes)=serde_json::to_vec(&row)else{return;};
    bytes.push(b'\n');
    if let Ok(mut file)=std::fs::OpenOptions::new().create(true).append(true).open(path){
        let _=file.write_all(&bytes);
    }
}
"""


def main():
    cli=argparse.ArgumentParser(description=__doc__)
    cli.add_argument('--root',type=pathlib.Path,default=pathlib.Path(__file__).resolve().parents[2])
    cli.add_argument('--output-dir',type=pathlib.Path,required=True)
    cli.add_argument('--trace',type=pathlib.Path,help='Optional isolated-worktree JSONL trace; never used to generate golden expectations')
    cli.add_argument('--trace-patch',type=pathlib.Path,help='Emit an optional patch for a separate disposable worktree; never changes source')
    args=cli.parse_args();parser,release,inventory=collect(args.root)
    outputs={'parser-title-expectations.json':parser,'release-300-parser-expectations.json':release,
        'source-test-inventory.json':inventory}
    if args.trace:outputs.update(summarize_trace(args.trace,{r['title'] for r in parser['cases']}))
    args.output_dir.mkdir(parents=True,exist_ok=True)
    if args.trace_patch:
        args.trace_patch.parent.mkdir(parents=True,exist_ok=True)
        args.trace_patch.write_text(trace_patch(args.root))
    for name,value in outputs.items():
        (args.output_dir/name).write_text(json.dumps(value,ensure_ascii=False,indent=2)+'\n')
    print(json.dumps(inventory['summary']))

if __name__=='__main__':main()
