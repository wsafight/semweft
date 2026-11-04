use super::*;
use std::time::Instant;

fn shell_arg(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn claude_run(ws: &Workspace, settings: &PathBuf, prompt: &str, label: &str, endpoint: &str) {
    let output = fs::File::create(ws.temp.path().join(format!("{label}.json"))).unwrap();
    let errors = fs::File::create(ws.temp.path().join(format!("{label}.stderr"))).unwrap();
    let mut child =
        Command::new(std::env::var_os("SEMWEFT_CLAUDE").unwrap_or_else(|| "claude".into()))
            .current_dir(&ws.root)
            .args([
                "-p",
                "--model",
                "haiku",
                "--output-format",
                "json",
                "--no-session-persistence",
                "--disable-slash-commands",
                "--setting-sources",
                "",
                "--strict-mcp-config",
                "--mcp-config",
                "{\"mcpServers\":{}}",
                "--permission-mode",
                "acceptEdits",
                "--tools",
                "Read,Edit",
                "--allowedTools",
                "Read,Edit",
                "--max-budget-usd",
                "1",
            ])
            .arg("--settings")
            .arg(settings)
            .arg(prompt)
            .env("SEMWEFT_CACHE_DIR", ws.temp.path().join("cache"))
            .env("SEMWEFT_CONFIG_DIR", ws.temp.path().join("config"))
            .env("SEMWEFT_HOST_ROOT", &ws.root)
            .env("SEMWEFT_HOST_EVENTS", ws.temp.path().join("events.jsonl"))
            .env("ANTHROPIC_BASE_URL", endpoint)
            .env("ANTHROPIC_API_KEY", "semweft-local-host-acceptance")
            .env_remove("CLAUDE_CODE_OAUTH_TOKEN")
            .env_remove("ANTHROPIC_AUTH_TOKEN")
            .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")
            .stdout(output)
            .stderr(errors)
            .spawn()
            .unwrap();
    let expires = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= expires {
            let _ = child.kill();
            let _ = child.wait();
            panic!("Claude host acceptance exceeded 60 seconds");
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    let result = fs::read_to_string(ws.temp.path().join(format!("{label}.json"))).unwrap();
    assert!(
        status.success(),
        "Claude host returned {status}: {result}; {}",
        fs::read_to_string(ws.temp.path().join(format!("{label}.stderr")))
            .unwrap()
            .chars()
            .take(2000)
            .collect::<String>()
    );
    let response: Value = serde_json::from_str(&result).unwrap();
    assert_ne!(response["is_error"], true, "{result}");
    eprintln!(
        "Claude local-model result ({label}): {}",
        response["result"]
    );
}

#[test]
#[ignore = "Requires Claude Code and Node.js; real host against a local scripted model, generated project only"]
fn real_claude_host_pre_post_receipts_and_protected_edits() {
    let ws = Workspace::new();
    fs::write(ws.root.join("src/lib.rs"), "pub fn stock() -> i32 { 1 }\n").unwrap();
    let server = ws.temp.path().join("model-server.cjs");
    fs::write(&server,r#"const http = require('node:http');
const fs = require('node:fs');
let turn = 0;
const server = http.createServer((req,res) => {
  const chunks=[];
  req.on('data', chunk => chunks.push(chunk));
  req.on('end', () => {
    if (!req.url.startsWith('/v1/messages') || req.url.includes('count_tokens')) {
      res.writeHead(200,{'content-type':'application/json'});res.end(JSON.stringify({input_tokens:10}));return;
    }
    const body=JSON.parse(Buffer.concat(chunks).toString());
    const step=turn++;
    const history=(body.messages||[]).filter(message=>message.role==='assistant').flatMap(message=>message.content||[]).filter(content=>content.type==='tool_use');
    const within=!body.tools?.some(tool=>tool.name==='Read') ? 2 : history.some(tool=>tool.name==='Edit') ? 2 : history.some(tool=>tool.name==='Read') ? 1 : 0;
    const phase=fs.readFileSync(process.env.SEMWEFT_HOST_FILE,'utf8').includes('{ 1 }') ? 0 : 1;
    const content=within===0 ? {type:'tool_use',id:'read_'+phase,name:'Read',input:{file_path:process.env.SEMWEFT_HOST_FILE}} : within===1 ? {type:'tool_use',id:'edit_'+phase,name:'Edit',input:{file_path:process.env.SEMWEFT_HOST_FILE,old_string:phase===0?'{ 1 }':'{ 2 }',new_string:phase===0?'{ 2 }':'{ 3 }'}} : {type:'text',text:'Done; hook result received.'};
    const message={id:'msg_local_'+step,type:'message',role:'assistant',content:[content],model:body.model,stop_reason:content.type==='tool_use'?'tool_use':'end_turn',stop_sequence:null,usage:{input_tokens:10,output_tokens:10}};
    if (!body.stream) {res.writeHead(200,{'content-type':'application/json'});res.end(JSON.stringify(message));return;}
    res.writeHead(200,{'content-type':'text/event-stream','cache-control':'no-cache'});
    const event=(type,data)=>res.write('event: '+type+'\ndata: '+JSON.stringify({type,...data})+'\n\n');
    event('message_start',{message:{...message,content:[],stop_reason:null,usage:{input_tokens:10,output_tokens:0}}});
    event('content_block_start',{index:0,content_block:content.type==='tool_use'?{...content,input:{}}:{type:'text',text:''}});
    event('content_block_delta',{index:0,delta:content.type==='tool_use'?{type:'input_json_delta',partial_json:JSON.stringify(content.input)}:{type:'text_delta',text:content.text}});
    event('content_block_stop',{index:0});
    event('message_delta',{delta:{stop_reason:message.stop_reason,stop_sequence:null},usage:{output_tokens:10}});
    event('message_stop',{});res.end();
  });
});
server.listen(0,'127.0.0.1',()=>console.log(server.address().port));
"#).unwrap();
    let mut mock = Command::new("node")
        .arg(&server)
        .env(
            "SEMWEFT_HOST_FILE",
            ws.root.canonicalize().unwrap().join("src/lib.rs"),
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut port = String::new();
    BufReader::new(mock.stdout.take().unwrap())
        .read_line(&mut port)
        .unwrap();
    struct StopMock(Child);
    impl Drop for StopMock {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let _mock = StopMock(mock);
    let endpoint = format!("http://127.0.0.1:{}", port.trim());
    let wrapper = ws.temp.path().join("hook-wrapper.cjs");
    let script = format!(
        r#"const fs = require('node:fs');
const {{spawnSync}} = require('node:child_process');
const input = fs.readFileSync(0, 'utf8');
const event = JSON.parse(input);
const result = spawnSync({}, ['--workspace', process.env.SEMWEFT_HOST_ROOT, 'hook', '--host', 'claude'], {{input, encoding:'utf8', timeout:15000}});
let response;
try {{ response = JSON.parse(result.stdout); }} catch {{ response = {{error:'invalid hook response'}}; }}
fs.appendFileSync(process.env.SEMWEFT_HOST_EVENTS, JSON.stringify({{event:event.hook_event_name, tool:event.tool_name, id:event.tool_use_id, session:event.session_id, response, status:result.status}}) + '\n');
process.stdout.write(result.stdout || '');
process.exit(result.status === 0 ? 0 : 2);
"#,
        serde_json::to_string(env!("CARGO_BIN_EXE_semweft")).unwrap()
    );
    fs::write(&wrapper, script).unwrap();
    let settings = ws.temp.path().join("hooks.json");
    let command = format!("node {}", shell_arg(wrapper.to_str().unwrap()));
    fs::write(&settings,serde_json::to_vec(&json!({"hooks":{
        "PreToolUse":[{"matcher":"Edit","hooks":[{"type":"command","command":command,"timeout":20}]}],
        "PostToolUse":[{"matcher":"Edit","hooks":[{"type":"command","command":command,"timeout":20}]}]
    }})).unwrap()).unwrap();
    claude_run(
        &ws,
        &settings,
        "Read src/lib.rs, then use the Edit tool exactly once to change `pub fn stock() -> i32 { 1 }` to `pub fn stock() -> i32 { 2 }`. This is a temporary acceptance fixture. Do not edit any other path or use any other tool. After the edit, reply done.",
        "allowed",
        &endpoint,
    );
    assert_eq!(
        fs::read_to_string(ws.root.join("src/lib.rs")).unwrap(),
        "pub fn stock() -> i32 { 2 }\n",
        "events: {}",
        fs::read_to_string(ws.temp.path().join("events.jsonl")).unwrap_or_default()
    );
    let events = fs::read_to_string(ws.temp.path().join("events.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    let before = events
        .iter()
        .find(|event| event["event"] == "PreToolUse")
        .expect("actual Claude host did not invoke PreToolUse");
    let after = events
        .iter()
        .find(|event| event["event"] == "PostToolUse")
        .expect("actual Claude host did not invoke PostToolUse");
    assert_eq!(before["id"], after["id"]);
    assert_eq!(before["session"], after["session"]);
    assert_eq!(before["status"], 0);
    assert_eq!(after["status"], 0);
    assert!(
        after["response"]["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .contains("Semweft PostToolUse:")
    );
    fs::write(ws.root.join("semweft.toml"),"schema_version=\"0.1\"\n[[guard.protected]]\nname=\"core\"\npaths=[\"src/lib.rs\"]\nreason=\"acceptance protected fixture\"\n").unwrap();
    claude_run(
        &ws,
        &settings,
        "Read src/lib.rs. Attempt exactly one Edit call changing `{ 2 }` to `{ 3 }`. If the hook denies it, stop and explain the denial. Do not retry, use alternate tools, or modify any other path.",
        "protected",
        &endpoint,
    );
    assert_eq!(
        fs::read_to_string(ws.root.join("src/lib.rs")).unwrap(),
        "pub fn stock() -> i32 { 2 }\n"
    );
    let events = fs::read_to_string(ws.temp.path().join("events.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert!(
        events
            .iter()
            .any(|event| event["response"]["hookSpecificOutput"]["permissionDecision"] == "deny")
    );
    eprintln!(
        "Actual Claude host with local scripted model: paired PreToolUse/PostToolUse and protected edit denial passed; generated temporary project only"
    );
}

#[test]
#[ignore = "Requires Codex CLI 0.166+ and Node.js; real host against a local scripted model, generated project only"]
fn real_codex_host_pre_post_receipts_and_protected_edits() {
    let ws = Workspace::new();
    let codex = std::env::var_os("SEMWEFT_CODEX").unwrap_or_else(|| "codex".into());
    let catalog = Command::new(&codex)
        .args(["debug", "models", "--bundled"])
        .output()
        .unwrap();
    assert!(catalog.status.success());
    let catalog: Value = serde_json::from_slice(&catalog.stdout).unwrap();
    let model = catalog["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|model| model["apply_patch_tool_type"] == "freeform")
        .expect("host catalog lacks a freeform apply_patch model")["slug"]
        .as_str()
        .unwrap();
    let model_config = format!("model={}", serde_json::to_string(model).unwrap());
    fs::write(ws.root.join("src/lib.rs"), "pub fn stock() -> i32 { 1 }\n").unwrap();
    let wrapper = ws.temp.path().join("codex-hook.cjs");
    fs::write(&wrapper,format!(r#"const fs=require('node:fs');
const {{spawnSync}}=require('node:child_process');
const input=fs.readFileSync(0,'utf8');const event=JSON.parse(input);
const result=spawnSync({},['--workspace',process.env.SEMWEFT_HOST_ROOT,'hook','--host','codex'],{{input,encoding:'utf8',timeout:15000}});
let response;try {{response=JSON.parse(result.stdout)}} catch {{response={{error:'invalid JSON'}}}}
fs.appendFileSync(process.env.SEMWEFT_HOST_EVENTS,JSON.stringify({{event:event.hook_event_name,tool:event.tool_name,id:event.tool_use_id,session:event.session_id,response,status:result.status}})+'\n');
process.stdout.write(result.stdout||'');process.exit(result.status===0?0:2);
"#,serde_json::to_string(env!("CARGO_BIN_EXE_semweft")).unwrap())).unwrap();
    let hook = format!("node {}", shell_arg(wrapper.to_str().unwrap()));
    let hook_matcher = format!(
        "[{{matcher=\"apply_patch\",hooks=[{{type=\"command\",command={},timeout=20}}]}}]",
        serde_json::to_string(&hook).unwrap()
    );
    let pre_hook = format!("hooks.PreToolUse={hook_matcher}");
    let post_hook = format!("hooks.PostToolUse={hook_matcher}");
    let server = ws.temp.path().join("codex-model.cjs");
    fs::write(&server,r#"const http=require('node:http');const fs=require('node:fs');let sequence=0;
const server=http.createServer((req,res)=>{const chunks=[];req.on('data',chunk=>chunks.push(chunk));req.on('end',()=>{
  const body=JSON.parse(Buffer.concat(chunks).toString());
  fs.appendFileSync(process.env.SEMWEFT_MODEL_LOG,JSON.stringify({input:(body.input||[]).filter(item=>['custom_tool_call','custom_tool_call_output','function_call','function_call_output'].includes(item.type))})+'\n');
  const done=(body.input||[]).some(item=>['custom_tool_call_output','function_call_output'].includes(item.type));
  const original=fs.readFileSync(process.env.SEMWEFT_HOST_FILE,'utf8').includes('{ 1 }')?'1':'2';
  const patch='*** Begin Patch\n*** Update File: src/lib.rs\n@@\n-pub fn stock() -> i32 { '+original+' }\n+pub fn stock() -> i32 { '+(Number(original)+1)+' }\n*** End Patch';
  const item=done?{id:'message_'+sequence,type:'message',role:'assistant',status:'completed',content:[{type:'output_text',text:'Done; hook decision observed.',annotations:[]}]}:{id:'item_'+sequence,type:'custom_tool_call',call_id:'edit_'+sequence,name:'apply_patch',input:patch};
  const response={id:'response_'+sequence++,object:'response',created_at:1793664000,status:'completed',output:[item],usage:{input_tokens:10,output_tokens:10,total_tokens:20}};
  res.writeHead(200,{'content-type':'text/event-stream','cache-control':'no-cache'});
  const event=(type,data)=>res.write('event: '+type+'\ndata: '+JSON.stringify({type,...data})+'\n\n');
  event('response.created',{response:{...response,status:'in_progress',output:[]}});
  event('response.output_item.added',{output_index:0,item});
  event('response.output_item.done',{output_index:0,item});
  event('response.completed',{response});res.end();
})});server.listen(0,'127.0.0.1',()=>console.log(server.address().port));
"#).unwrap();
    let mut mock = Command::new("node")
        .arg(&server)
        .env(
            "SEMWEFT_HOST_FILE",
            ws.root.canonicalize().unwrap().join("src/lib.rs"),
        )
        .env(
            "SEMWEFT_MODEL_LOG",
            ws.temp.path().join("codex-model.jsonl"),
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut port = String::new();
    BufReader::new(mock.stdout.take().unwrap())
        .read_line(&mut port)
        .unwrap();
    struct StopMock(Child);
    impl Drop for StopMock {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let _mock = StopMock(mock);
    for label in ["allowed", "protected"] {
        if label == "protected" {
            fs::write(ws.root.join("semweft.toml"),"schema_version=\"0.1\"\n[[guard.protected]]\nname=\"core\"\npaths=[\"src/lib.rs\"]\nreason=\"host acceptance\"\n").unwrap();
        }
        let stdout = fs::File::create(ws.temp.path().join(format!("codex-{label}.jsonl"))).unwrap();
        let stderr =
            fs::File::create(ws.temp.path().join(format!("codex-{label}.stderr"))).unwrap();
        let provider = format!(
            "model_providers.semweft_local={{name=\"Semweft local protocol fixture\",base_url=\"http://127.0.0.1:{}/v1\",wire_api=\"responses\",requires_openai_auth=false}}",
            port.trim()
        );
        let mut child=Command::new(&codex).current_dir(&ws.root)
            .args(["exec","--ignore-user-config","--ignore-rules","--ephemeral","--skip-git-repo-check","--dangerously-bypass-hook-trust","--json","--sandbox","workspace-write",
                "-c","approval_policy=\"never\"","-c","model_provider=\"semweft_local\"","-c",&model_config,"-c","features.hooks=true","-c","features.multi_agent=false","-c"])
            .arg(provider).arg("-c").arg(&pre_hook).arg("-c").arg(&post_hook).arg("Apply the supplied one-line patch in this temporary fixture; stop if the hook denies it.")
            .env("SEMWEFT_HOST_ROOT",&ws.root).env("SEMWEFT_HOST_EVENTS",ws.temp.path().join("codex-events.jsonl"))
            .env("SEMWEFT_CACHE_DIR",ws.temp.path().join("cache")).env("SEMWEFT_CONFIG_DIR",ws.temp.path().join("config"))
            .stdout(stdout).stderr(stderr).spawn().unwrap();
        let expires = Instant::now() + Duration::from_secs(60);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= expires {
                let _ = child.kill();
                let _ = child.wait();
                panic!("Codex host acceptance exceeded 60 seconds");
            }
            std::thread::sleep(Duration::from_millis(25));
        };
        assert!(
            status.success(),
            "Codex {label}: {}; {}",
            fs::read_to_string(ws.temp.path().join(format!("codex-{label}.jsonl"))).unwrap(),
            fs::read_to_string(ws.temp.path().join(format!("codex-{label}.stderr"))).unwrap()
        );
        assert_eq!(
            fs::read_to_string(ws.root.join("src/lib.rs")).unwrap(),
            "pub fn stock() -> i32 { 2 }\n",
            "Codex {label}: {}; model: {}; events: {}",
            fs::read_to_string(ws.temp.path().join(format!("codex-{label}.jsonl"))).unwrap(),
            fs::read_to_string(ws.temp.path().join("codex-model.jsonl")).unwrap_or_default(),
            fs::read_to_string(ws.temp.path().join("codex-events.jsonl")).unwrap_or_default()
        );
    }
    let events = fs::read_to_string(ws.temp.path().join("codex-events.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    let before = events
        .iter()
        .find(|event| event["event"] == "PreToolUse")
        .unwrap();
    let after = events
        .iter()
        .find(|event| event["event"] == "PostToolUse")
        .unwrap();
    assert_eq!(before["id"], after["id"]);
    assert_eq!(before["session"], after["session"]);
    assert!(before["session"].is_string());
    assert_eq!(after["status"], 0);
    assert!(
        after["response"]["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .contains("Semweft PostToolUse:")
    );
    assert!(
        events
            .iter()
            .any(|event| event["response"]["hookSpecificOutput"]["permissionDecision"] == "deny")
    );
    eprintln!(
        "Actual Codex host with local scripted model: paired PreToolUse/PostToolUse and protected edit denial passed"
    );
}
