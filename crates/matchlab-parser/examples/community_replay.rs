//! Current-source parser replay for Community Learning intake.
//! Each stdin line is a JSON-encoded release title. Never invokes metadata,
//! shell commands, network requests or untrusted code.
use std::io::{self,BufRead};

fn main(){
    for line in io::stdin().lock().lines(){
        let result=match line {
            Ok(line)=>{
                match serde_json::from_str::<String>(&line) {
                    Ok(title) if title.len()<=240=>{
                        match matchlab_parser::parse_title(&title,false){
                            Ok(parsed)=>serde_json::json!({"ok":true,"parsed":parsed}),
                            Err(_)=>serde_json::json!({"ok":false,"parsed":null})
                        }
                    },
                    _=>serde_json::json!({"ok":false,"parsed":null})
                }
            },
            Err(_)=>break
        };
        println!("{result}");
    }
}