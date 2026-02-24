//! Setup wizard for the MisebanAI camera agent.
//!
//! Serves a single-page web UI on port 3939 that guides the user through:
//! 1. Entering a 6-digit pairing code
//! 2. Scanning the local network for cameras
//! 3. Selecting cameras and saving config

use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tracing::{error, info};

use crate::scanner;

#[derive(Clone)]
struct AppState {
    http: reqwest::Client,
    paired: Arc<Mutex<Option<PairResult>>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PairRequest {
    code: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PairResult {
    token: String,
    store_id: String,
    store_name: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct SaveRequest {
    cameras: Vec<SelectedCamera>,
}

#[derive(Debug, Serialize, Deserialize)]
struct SelectedCamera {
    id: String,
    name: String,
    url: String,
    mode: String,
    interval_secs: u64,
}

#[derive(Debug, Serialize)]
struct ApiResponse<T: Serialize> {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl<T: Serialize> ApiResponse<T> {
    fn success(data: T) -> Self {
        Self {
            ok: true,
            data: Some(data),
            error: None,
        }
    }
    fn err(msg: impl Into<String>) -> Self {
        Self {
            ok: false,
            data: None,
            error: Some(msg.into()),
        }
    }
}

fn config_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".miseban")
}

fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

async fn index_page() -> Html<&'static str> {
    Html(SETUP_HTML)
}

async fn handle_pair(
    State(state): State<AppState>,
    Json(req): Json<PairRequest>,
) -> impl IntoResponse {
    let code = req.code.trim().to_string();
    if code.len() != 6 || !code.chars().all(|c| c.is_ascii_digit()) {
        return (
            StatusCode::BAD_REQUEST,
            Json(ApiResponse::<PairResult>::err(
                "ペアリングコードは6桁の数字です",
            )),
        );
    }

    info!(code = %code, "Attempting pairing with cloud API");
    let payload = serde_json::json!({ "code": code });
    let result = state
        .http
        .post("https://api.misebanai.com/v1/pair")
        .json(&payload)
        .send()
        .await;

    match result {
        Ok(resp) if resp.status().is_success() => match resp.json::<PairResult>().await {
            Ok(pair) => {
                info!(store_id = %pair.store_id, store_name = %pair.store_name, "Pairing successful");
                *state.paired.lock().await = Some(pair.clone());
                (StatusCode::OK, Json(ApiResponse::success(pair)))
            }
            Err(e) => {
                error!(error = %e, "Failed to parse pairing response");
                (StatusCode::BAD_GATEWAY, Json(ApiResponse::<PairResult>::err(
                        "サーバーからの応答を解析できませんでした。しばらくしてからお試しください。",
                    )))
            }
        },
        Ok(resp) => {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            error!(status = %status, body = %body, "Pairing API returned error");
            let msg = if status == StatusCode::NOT_FOUND || status == StatusCode::BAD_REQUEST {
                "ペアリングコードが正しくないか、期限切れです。ダッシュボードで新しいコードを発行してください。"
            } else {
                "サーバーに接続できませんでした。ネットワーク接続を確認してください。"
            };
            (
                StatusCode::BAD_REQUEST,
                Json(ApiResponse::<PairResult>::err(msg)),
            )
        }
        Err(e) => {
            error!(error = %e, "Failed to reach pairing API");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ApiResponse::<PairResult>::err(
                    "クラウドAPIに接続できません。ネットワーク接続を確認してください。",
                )),
            )
        }
    }
}

async fn handle_scan() -> impl IntoResponse {
    info!("Starting network camera scan from setup wizard");
    let cameras = scanner::scan_network().await;
    Json(ApiResponse::success(cameras))
}

async fn handle_save(
    State(state): State<AppState>,
    Json(req): Json<SaveRequest>,
) -> impl IntoResponse {
    let paired = state.paired.lock().await;
    let pair = match paired.as_ref() {
        Some(p) => p.clone(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(ApiResponse::<String>::err(
                    "先にペアリングコードを入力してください",
                )),
            )
        }
    };
    drop(paired);

    if req.cameras.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(ApiResponse::<String>::err(
                "カメラを1台以上選択してください",
            )),
        );
    }

    let mut toml_str = String::new();
    toml_str.push_str("# MisebanAI Agent Config (auto-generated by setup wizard)\n\n[server]\n");
    toml_str.push_str("endpoint = \"https://api.miseban.ai/v1/frames\"\n");
    toml_str.push_str(&format!("token = \"{}\"\n", pair.token));
    toml_str.push_str(&format!("# store_id = \"{}\"\n", pair.store_id));
    toml_str.push_str(&format!("# store_name = \"{}\"\n\n", pair.store_name));
    for cam in &req.cameras {
        toml_str.push_str(&format!(
            "[[cameras]]\nid = \"{}\"\nname = \"{}\"\nmode = \"{}\"\nurl = \"{}\"\ninterval_secs = {}\n\n",
            cam.id, cam.name, cam.mode, cam.url, cam.interval_secs
        ));
    }

    let dir = config_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        error!(error = %e, path = %dir.display(), "Failed to create config directory");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiResponse::<String>::err(
                "設定ディレクトリを作成できませんでした",
            )),
        );
    }

    let path = config_path();
    match std::fs::write(&path, &toml_str) {
        Ok(()) => {
            info!(path = %path.display(), cameras = req.cameras.len(), "Config saved");
            (
                StatusCode::OK,
                Json(ApiResponse::success(format!(
                    "設定を {} に保存しました。エージェントを再起動してください。",
                    path.display()
                ))),
            )
        }
        Err(e) => {
            error!(error = %e, path = %path.display(), "Failed to write config");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiResponse::<String>::err(format!(
                    "設定ファイルの書き込みに失敗しました: {}",
                    e
                ))),
            )
        }
    }
}

/// Starts the setup wizard web server on port 3939.
pub async fn run_setup_server() -> Result<(), Box<dyn std::error::Error>> {
    let state = AppState {
        http: reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()?,
        paired: Arc::new(Mutex::new(None)),
    };

    let app = Router::new()
        .route("/", get(index_page))
        .route("/api/pair", post(handle_pair))
        .route("/api/scan", get(handle_scan))
        .route("/api/save", post(handle_save))
        .with_state(state);

    let addr = "0.0.0.0:3939";
    info!("Setup wizard running at http://0.0.0.0:3939");
    println!();
    println!("  +----------------------------------------------+");
    println!("  |  MisebanAI セットアップウィザード             |");
    println!("  |  ブラウザで以下を開いてください:              |");
    println!("  |  http://<このデバイスのIP>:3939               |");
    println!("  +----------------------------------------------+");
    println!();

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

const SETUP_HTML: &str = r##"<!DOCTYPE html>
<html lang="ja">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1,maximum-scale=1">
<title>MisebanAI セットアップ</title>
<style>
*{margin:0;padding:0;box-sizing:border-box}
body{font-family:-apple-system,BlinkMacSystemFont,'Hiragino Sans',sans-serif;background:#f0f2f5;color:#1a1a2e;min-height:100vh;display:flex;justify-content:center;align-items:flex-start;padding:24px 16px}
.container{max-width:480px;width:100%}
.logo{text-align:center;margin-bottom:32px}
.logo h1{font-size:24px;color:#4338ca;font-weight:800;letter-spacing:-0.5px}
.logo p{font-size:13px;color:#6b7280;margin-top:4px}
.card{background:#fff;border-radius:16px;padding:32px 24px;box-shadow:0 1px 3px rgba(0,0,0,.08),0 8px 24px rgba(0,0,0,.04);margin-bottom:16px}
.steps{display:flex;justify-content:center;gap:8px;margin-bottom:28px}
.step-dot{width:10px;height:10px;border-radius:50%;background:#e5e7eb;transition:all .3s}
.step-dot.active{background:#4338ca;transform:scale(1.2)}
.step-dot.done{background:#22c55e}
h2{font-size:18px;font-weight:700;text-align:center;margin-bottom:8px}
.subtitle{font-size:14px;color:#6b7280;text-align:center;margin-bottom:24px}
.code-inputs{display:flex;justify-content:center;gap:8px;margin-bottom:24px}
.code-inputs input{width:48px;height:56px;text-align:center;font-size:24px;font-weight:700;border:2px solid #e5e7eb;border-radius:12px;outline:none;transition:border .2s;caret-color:#4338ca}
.code-inputs input:focus{border-color:#4338ca;box-shadow:0 0 0 3px rgba(67,56,202,.12)}
.btn{display:block;width:100%;padding:14px;border:none;border-radius:12px;font-size:16px;font-weight:700;cursor:pointer;transition:all .15s}
.btn-primary{background:#4338ca;color:#fff}
.btn-primary:hover{background:#3730a3}
.btn-primary:disabled{background:#a5b4fc;cursor:not-allowed}
.btn-outline{background:#fff;color:#4338ca;border:2px solid #e5e7eb}
.btn-outline:hover{border-color:#4338ca;background:#eef2ff}
.error-msg{background:#fef2f2;color:#dc2626;padding:12px 16px;border-radius:10px;font-size:13px;margin-bottom:16px;display:none;text-align:center}
.scanning{text-align:center;padding:24px 0}
.spinner{width:48px;height:48px;border:4px solid #e5e7eb;border-top-color:#4338ca;border-radius:50%;animation:spin 1s linear infinite;margin:0 auto 16px}
@keyframes spin{to{transform:rotate(360deg)}}
.cam-list{list-style:none;margin-bottom:24px}
.cam-item{display:flex;align-items:center;gap:12px;padding:14px 16px;border:2px solid #e5e7eb;border-radius:12px;margin-bottom:8px;cursor:pointer;transition:all .15s}
.cam-item:hover{border-color:#a5b4fc;background:#eef2ff}
.cam-item.selected{border-color:#4338ca;background:#eef2ff}
.cam-item input[type=checkbox]{width:20px;height:20px;accent-color:#4338ca}
.cam-info{flex:1;min-width:0}
.cam-name{font-size:15px;font-weight:600}
.cam-url{font-size:12px;color:#6b7280;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
.cam-badge{font-size:11px;font-weight:700;padding:2px 8px;border-radius:6px;text-transform:uppercase}
.cam-badge.rtsp{background:#dbeafe;color:#2563eb}
.cam-badge.http{background:#dcfce7;color:#16a34a}
.success-icon{width:80px;height:80px;background:#dcfce7;border-radius:50%;display:flex;align-items:center;justify-content:center;margin:0 auto 20px}
.success-icon svg{width:40px;height:40px;color:#22c55e}
.no-cameras{text-align:center;padding:16px;color:#6b7280;font-size:14px}
.step{display:none}.step.active{display:block}
.pair-info{background:#eef2ff;border-radius:10px;padding:12px 16px;margin-bottom:20px;text-align:center;font-size:14px;color:#4338ca}
</style>
</head>
<body>
<div class="container">
  <div class="logo">
    <h1>MisebanAI</h1>
    <p>カメラエージェント セットアップ</p>
  </div>
  <div class="card">
    <div class="steps">
      <div class="step-dot active" id="dot-0"></div>
      <div class="step-dot" id="dot-1"></div>
      <div class="step-dot" id="dot-2"></div>
    </div>
    <!-- Step 1: Pairing Code -->
    <div class="step active" id="step-0">
      <h2>ペアリングコード入力</h2>
      <p class="subtitle">ダッシュボードに表示された6桁のコードを入力してください</p>
      <div class="error-msg" id="pair-error"></div>
      <div class="code-inputs" id="code-inputs">
        <input type="text" inputmode="numeric" maxlength="1" autocomplete="off">
        <input type="text" inputmode="numeric" maxlength="1" autocomplete="off">
        <input type="text" inputmode="numeric" maxlength="1" autocomplete="off">
        <input type="text" inputmode="numeric" maxlength="1" autocomplete="off">
        <input type="text" inputmode="numeric" maxlength="1" autocomplete="off">
        <input type="text" inputmode="numeric" maxlength="1" autocomplete="off">
      </div>
      <button class="btn btn-primary" id="btn-pair" disabled>ペアリング</button>
    </div>
    <!-- Step 2: Camera Scan -->
    <div class="step" id="step-1">
      <h2>カメラを検出中</h2>
      <p class="subtitle" id="scan-subtitle">ネットワーク上のカメラを探しています...</p>
      <div class="error-msg" id="scan-error"></div>
      <div class="scanning" id="scanning-spinner">
        <div class="spinner"></div>
        <p style="color:#6b7280;font-size:14px">スキャン中... しばらくお待ちください</p>
      </div>
      <div id="cam-results" style="display:none">
        <ul class="cam-list" id="cam-list"></ul>
        <button class="btn btn-primary" id="btn-save" disabled>選択したカメラで設定を保存</button>
        <button class="btn btn-outline" id="btn-rescan" style="margin-top:8px">再スキャン</button>
      </div>
      <div id="no-cam-results" style="display:none">
        <div class="no-cameras">
          <p style="font-size:32px;margin-bottom:12px">📷</p>
          <p>カメラが見つかりませんでした</p>
          <p style="margin-top:4px;font-size:12px">カメラがネットワークに接続されているか確認してください</p>
        </div>
        <button class="btn btn-outline" id="btn-rescan2" style="margin-top:16px">再スキャン</button>
      </div>
    </div>
    <!-- Step 3: Complete -->
    <div class="step" id="step-2">
      <div class="success-icon">
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="3" stroke-linecap="round" stroke-linejoin="round"><polyline points="20 6 9 17 4 12"/></svg>
      </div>
      <h2>セットアップ完了!</h2>
      <p class="subtitle" id="done-msg">設定が保存されました。エージェントを再起動すると撮影が開始されます。</p>
      <div class="pair-info" id="done-store-info"></div>
      <p style="text-align:center;font-size:13px;color:#6b7280;margin-top:16px">
        設定ファイル: <code>~/.miseban/config.toml</code>
      </p>
    </div>
  </div>
</div>
<script>
(function(){
  var inputs=document.querySelectorAll('#code-inputs input');
  var btnPair=document.getElementById('btn-pair');
  var pairError=document.getElementById('pair-error');

  inputs.forEach(function(inp,i){
    inp.addEventListener('input',function(){
      this.value=this.value.replace(/\D/g,'').slice(0,1);
      if(this.value&&i<inputs.length-1)inputs[i+1].focus();
      updatePairBtn();
    });
    inp.addEventListener('keydown',function(e){
      if(e.key==='Backspace'&&!this.value&&i>0){inputs[i-1].focus();inputs[i-1].value='';updatePairBtn();}
    });
    inp.addEventListener('paste',function(e){
      e.preventDefault();
      var t=(e.clipboardData||window.clipboardData).getData('text').replace(/\D/g,'');
      for(var j=0;j<Math.min(t.length,inputs.length);j++)inputs[j].value=t[j];
      inputs[Math.min(t.length,inputs.length-1)].focus();
      updatePairBtn();
    });
  });

  function getCode(){var c='';inputs.forEach(function(inp){c+=inp.value});return c;}
  function updatePairBtn(){btnPair.disabled=getCode().length!==6;}
  function showError(el,msg){el.textContent=msg;el.style.display='block';}
  function hideError(el){el.style.display='none';}

  function goToStep(n){
    document.querySelectorAll('.step').forEach(function(s,i){s.classList.toggle('active',i===n);});
    for(var i=0;i<3;i++){
      var d=document.getElementById('dot-'+i);
      d.classList.remove('active','done');
      if(i<n)d.classList.add('done');else if(i===n)d.classList.add('active');
    }
  }

  btnPair.addEventListener('click',function(){
    var code=getCode();if(code.length!==6)return;
    hideError(pairError);btnPair.disabled=true;btnPair.textContent='接続中...';
    fetch('/api/pair',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({code:code})})
    .then(function(r){return r.json();})
    .then(function(res){
      if(res.ok){window._paired=res.data;goToStep(1);startScan();}
      else{showError(pairError,res.error||'ペアリングに失敗しました');btnPair.disabled=false;btnPair.textContent='ペアリング';}
    })
    .catch(function(){showError(pairError,'ネットワークエラーが発生しました');btnPair.disabled=false;btnPair.textContent='ペアリング';});
  });

  function startScan(){
    var spinner=document.getElementById('scanning-spinner'),results=document.getElementById('cam-results');
    var noResults=document.getElementById('no-cam-results'),scanError=document.getElementById('scan-error');
    spinner.style.display='block';results.style.display='none';noResults.style.display='none';hideError(scanError);
    document.getElementById('scan-subtitle').textContent='ネットワーク上のカメラを探しています...';
    fetch('/api/scan').then(function(r){return r.json();})
    .then(function(res){
      spinner.style.display='none';
      if(res.ok&&res.data&&res.data.length>0){
        document.getElementById('scan-subtitle').textContent=res.data.length+'台のカメラが見つかりました';
        renderCameras(res.data);results.style.display='block';
      }else{document.getElementById('scan-subtitle').textContent='カメラの検出結果';noResults.style.display='block';}
    })
    .catch(function(){spinner.style.display='none';showError(scanError,'スキャン中にエラーが発生しました');});
  }

  function renderCameras(cameras){
    var list=document.getElementById('cam-list');list.innerHTML='';window._cameras=cameras;
    cameras.forEach(function(cam,idx){
      var li=document.createElement('li');li.className='cam-item';
      li.innerHTML='<input type="checkbox" data-idx="'+idx+'"><div class="cam-info"><div class="cam-name">'+esc(cam.name)+'</div><div class="cam-url">'+esc(cam.url)+'</div></div><span class="cam-badge '+cam.protocol+'">'+cam.protocol+'</span>';
      li.addEventListener('click',function(e){
        if(e.target.tagName==='INPUT')return;
        var cb=li.querySelector('input[type=checkbox]');cb.checked=!cb.checked;
        li.classList.toggle('selected',cb.checked);updateSaveBtn();
      });
      li.querySelector('input').addEventListener('change',function(){li.classList.toggle('selected',this.checked);updateSaveBtn();});
      list.appendChild(li);
    });
  }

  function esc(s){var d=document.createElement('div');d.textContent=s;return d.innerHTML;}
  function updateSaveBtn(){document.getElementById('btn-save').disabled=document.querySelectorAll('#cam-list input:checked').length===0;}

  document.getElementById('btn-rescan').addEventListener('click',startScan);
  document.getElementById('btn-rescan2').addEventListener('click',startScan);

  document.getElementById('btn-save').addEventListener('click',function(){
    var checked=document.querySelectorAll('#cam-list input:checked'),selected=[];
    checked.forEach(function(cb){
      var cam=window._cameras[parseInt(cb.dataset.idx)];
      selected.push({id:'cam-'+(parseInt(cb.dataset.idx)+1),name:cam.name,url:cam.url,mode:cam.protocol==='rtsp'?'rtsp':'snapshot',interval_secs:5});
    });
    var btnSave=document.getElementById('btn-save');
    btnSave.disabled=true;btnSave.textContent='保存中...';
    fetch('/api/save',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({cameras:selected})})
    .then(function(r){return r.json();})
    .then(function(res){
      if(res.ok){
        var info=document.getElementById('done-store-info');
        if(window._paired)info.textContent=window._paired.store_name+' ('+selected.length+'台のカメラ)';
        goToStep(2);
      }else{showError(document.getElementById('scan-error'),res.error||'保存に失敗しました');btnSave.disabled=false;btnSave.textContent='選択したカメラで設定を保存';}
    })
    .catch(function(){showError(document.getElementById('scan-error'),'保存中にエラーが発生しました');btnSave.disabled=false;btnSave.textContent='選択したカメラで設定を保存';});
  });

  inputs[0].focus();
})();
</script>
</body>
</html>"##;
