//! Tiny ONNX graph writer in pure Rust (no Python, no training runtime).
//! A bounded 8-feature logistic ranker is represented by MatMul + Add +
//! Sigmoid. Weights are conservative bootstrap priors, NOT training truth.
//! Future versions can replace the model only through signed/verified
//! updates; this model never changes production matching decisions.
#![cfg(feature="ml-ort")]
use std::path::Path;

const INPUT:usize=8;
const IR_VERSION:u64=9;
const OPSET:u64=13;
const WEIGHTS:[f32;INPUT]=[
    -0.7, // year differences are usually not proven errors
     1.3, // missing verified episode
     1.2, // missing verified season
     0.9, // mismatched episode
     0.8, // mismatched season
     1.1, // missing anime episode
     0.1, // missing parser field is just a hypothesis
     0.8  // repeated independent evidence
];
const BIAS:f32=-2.5;
fn vint(mut n:u64,out:&mut Vec<u8>){
    while n>=128{out.push((n as u8&0x7f)|0x80);n>>=7;}
    out.push(n as u8);
}
fn field_var(n:u64,v:u64,out:&mut Vec<u8>){
    vint(n<<3,out);vint(v,out);
}
fn field_bytes(n:u64,value:&[u8],out:&mut Vec<u8>){
    vint((n<<3)|2,out);vint(value.len() as u64,out);out.extend_from_slice(value);
}
fn field_str(n:u64,text:&str,out:&mut Vec<u8>){field_bytes(n,text.as_bytes(),out);}
fn node(name:&str,op:&str,inputs:&[&str],outputs:&[&str])->Vec<u8>{
    let mut out=Vec::new();
    for v in inputs{field_str(1,v,&mut out);}
    for v in outputs{field_str(2,v,&mut out);}
    field_str(3,name,&mut out);
    field_str(4,op,&mut out);
    out
}
fn tensor(name:&str,dims:&[u64],values:&[f32])->Vec<u8>{
    let mut out=Vec::new();
    for n in dims{field_var(1,*n,&mut out);}
    field_var(2,1,&mut out); // FLOAT
    field_str(8,name,&mut out);
    let raw=values.iter().flat_map(|x|x.to_le_bytes()).collect::<Vec<_>>();
    field_bytes(9,&raw,&mut out);
    out
}
fn value_info(name:&str,dims:&[u64])->Vec<u8>{
    let mut shape=Vec::new();
    for dim in dims{
        let mut value=Vec::new();
        field_var(1,*dim,&mut value); // TensorShapeProto.Dimension.dim_value
        field_bytes(1,&value,&mut shape);
    }
    let mut tensor_type=Vec::new();
    field_var(1,1,&mut tensor_type); // elem_type FLOAT
    field_bytes(2,&shape,&mut tensor_type);
    let mut full_type=Vec::new();
    field_bytes(1,&tensor_type,&mut full_type); // TypeProto.tensor_type
    let mut out=Vec::new();
    field_str(1,name,&mut out);
    field_bytes(2,&full_type,&mut out);
    out
}
pub fn model()->Vec<u8>{
    let mut graph=Vec::new();
    for (name,op,inputs,outputs) in [
        ("dense","MatMul",vec!["features","W"],vec!["linear"]),
        ("bias","Add",vec!["linear","B"],vec!["logits"]),
        ("probability","Sigmoid",vec!["logits"],vec!["score"])
    ]{
        field_bytes(1,&node(name,op,&inputs,&outputs),&mut graph);
    }
    field_str(2,"MatchLabTinyAnomalyRanker",&mut graph);
    field_bytes(5,&tensor("W",&[INPUT as u64,1],&WEIGHTS),&mut graph);
    field_bytes(5,&tensor("B",&[1],&[BIAS]),&mut graph);
    field_bytes(11,&value_info("features",&[1,INPUT as u64]),&mut graph);
    field_bytes(12,&value_info("score",&[1,1]),&mut graph);
    let mut model=Vec::new();
    field_var(1,IR_VERSION,&mut model);
    field_str(2,"MatchLab (Rust)",&mut model);
    field_str(3,"1",&mut model);
    field_bytes(7,&graph,&mut model);
    let mut opset=Vec::new();
    field_var(2,OPSET,&mut opset);
    field_bytes(8,&opset,&mut model);
    model
}
pub fn ensure(path:&Path)->Result<(),String>{
    if path.exists(){
        let size=path.metadata().map_err(|e|e.to_string())?.len();
        if size==0||size>4*1024*1024{return Err("invalid ONNX model file size".into());}
        return Ok(());
    }
    let data=model();
    if let Some(parent)=path.parent(){
        std::fs::create_dir_all(parent).map_err(|e|e.to_string())?;
    }
    let temp=path.with_extension("onnx.tmp");
    std::fs::write(&temp,data).map_err(|e|e.to_string())?;
    std::fs::rename(&temp,path).map_err(|e|e.to_string())
}
#[cfg(test)]
mod tests{
    use super::*;
    #[test]
    fn bootstrap_onnx_remains_tiny_and_deterministic(){
        let bytes=model();
        assert!(bytes.len()<2048,"{} bytes",bytes.len());
        assert_eq!(bytes,model());
        assert!(bytes.windows(7).any(|w|w==b"Sigmoid"));
        assert!(bytes.windows(6).any(|w|w==b"MatMul"));
    }
}
