//! Browser pages: a dev sign-in page and a live lobby viewer.

use axum::{extract::State, http::StatusCode, response::Html};

use crate::AppState;

const STYLE: &str = r#"<style>
:root{--bg:#f6f6f4;--card:#fff;--ink:#16181d;--muted:#6b7080;--line:#e3e3df;--accent:#2a5bd7;--warn:#a15c00}
@media (prefers-color-scheme:dark){:root{--bg:#121316;--card:#1b1d22;--ink:#e9eaee;--muted:#9097a6;--line:#2b2e35;--accent:#7aa2ff;--warn:#f0b35a}}
*{box-sizing:border-box}body{font:15px/1.45 system-ui,sans-serif;max-width:860px;margin:auto;padding:24px 16px;background:var(--bg);color:var(--ink)}
h1{margin:0 0 4px;font-size:22px}.muted{color:var(--muted);font-size:13px}.card{background:var(--card);border:1px solid var(--line);border-radius:10px;padding:14px 16px;margin:10px 0}
.tag{display:inline-block;border:1px solid var(--line);border-radius:6px;padding:1px 7px;margin:2px 4px 0 0;font-size:12px}
.warn{color:var(--warn)}button{font:inherit;border:1px solid var(--line);background:var(--card);color:var(--ink);border-radius:7px;padding:4px 10px;margin:4px 4px 0 0;cursor:pointer}
button.on{border-color:var(--accent);color:var(--accent);font-weight:600}input{font:inherit;padding:5px 8px;border:1px solid var(--line);border-radius:7px;background:var(--card);color:var(--ink)}
.row{display:flex;gap:12px;align-items:baseline;justify-content:space-between;flex-wrap:wrap}.score{font-variant-numeric:tabular-nums;font-weight:600}a{color:var(--accent)}
</style>"#;

const ESCAPE: &str = r#"function esc(v){return String(v).replace(/[&<>"']/g,c=>({"&":"&amp;","<":"&lt;",">":"&gt;","\"":"&quot;","'":"&#039;"}[c]));}"#;

/// Sign in as any test account. Exists only when AUTH_DEV_LOGIN=1.
pub async fn dev(State(state): State<AppState>) -> Result<Html<String>, StatusCode> {
    if !state.auth.dev_login { return Err(StatusCode::NOT_FOUND); }
    Ok(Html(format!(r#"<!DOCTYPE html><html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>Dev Sign-in</title>{STYLE}</head><body>
<h1>Dev sign-in</h1><div class="muted">Local development only: sign in as any test account.</div>
<div class="card" id="who"></div>
<div class="card"><input id="user" placeholder="user id, e.g. dev-you"> <button onclick="signIn(document.getElementById('user').value)">Sign in</button><div id="quick"></div></div>
<h3>Lobbies</h3><div id="lobbies"></div>
<script>{ESCAPE}
async function signIn(u){{const r=await fetch("/auth/dev/login",{{method:"POST",headers:{{"Content-Type":"application/json"}},body:JSON.stringify({{user_id:u}})}});
if(!r.ok){{alert(await r.text()||r.status);return;}}load();}}
async function load(){{const me=await fetch("/api/me");document.getElementById("who").innerHTML=me.ok?`Signed in as <b>${{esc((await me.json()).user_id)}}</b>`:"Not signed in";
const users=(new URLSearchParams(location.search).get("users")||"").split(",").filter(Boolean);
document.getElementById("quick").innerHTML=users.map(u=>`<button onclick="signIn('${{esc(u)}}')">${{esc(u)}}</button>`).join("");
const ls=await (await fetch("/api/lobbies")).json();
document.getElementById("lobbies").innerHTML=ls.map(l=>`<div class="card"><a href="/lobbies/${{esc(l.id)}}">${{esc(l.name)}}</a> <span class="muted">${{esc(l.id)}} · owner ${{esc(l.owner)}} · ${{l.members}} members · v${{l.version}}</span></div>`).join("")||'<div class="muted">none yet</div>';}}
load();
</script></body></html>"#)))
}

/// Live view of a lobby for the signed-in member: refetches on every event and lets you vote.
pub async fn lobby() -> Html<String> {
    Html(format!(r#"<!DOCTYPE html><html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>Lobby Feed</title>{STYLE}</head><body>
<div class="row"><h1 id="title">Lobby</h1><span class="muted" id="status"></span></div>
<div class="muted" id="who"></div><div id="locked"></div><div id="feed"></div>
<script>{ESCAPE}
const id=decodeURIComponent(location.pathname.split("/").pop());
const KINDS=["like","bookmark","reply","skip"];
async function api(p,o){{const r=await fetch(p,o);if(!r.ok)throw new Error(r.status+" "+await r.text());return r.json();}}
async function vote(item,kind,active){{await api(`/api/lobbies/${{encodeURIComponent(id)}}/interactions`,{{method:"POST",headers:{{"Content-Type":"application/json"}},body:JSON.stringify({{item_id:item,kind,active}})}});}}
function render(f){{
 document.getElementById("status").textContent=`v${{f.version}} · ${{f.items.length}} posts`;
 document.getElementById("locked").innerHTML=f.locked.map(l=>`<div class="card muted">Locked: <b>${{esc(l.label)}}</b> — progress ${{esc(JSON.stringify(l.progress))}}</div>`).join("");
 document.getElementById("feed").innerHTML=f.items.map((i,n)=>`<div class="card">
  <div class="row"><span class="muted">#${{n+1}} · ${{esc(i.author)}} · ${{i.contributors}} feed${{i.contributors==1?"":"s"}} · ${{i.likes}} likes${{i.seen?" · seen":""}}${{i.explore?" · exploring":""}}</span>
  <span class="score" title="consensus ${{i.global.toFixed(2)}} · like-minded ${{i.personal.toFixed(2)}}">${{i.score.toFixed(2)}}</span></div>
  <div>${{esc(i.text)}}</div>
  <div>${{i.categories.map(c=>`<span class="tag">${{esc(c)}}</span>`).join("")}}${{i.amplified?"":'<span class="tag warn">finance: not amplified</span>'}}</div>
  <div>${{KINDS.map(k=>{{const on=i.mine.includes(k);return `<button class="${{on?"on":""}}" onclick="vote('${{i.id}}','${{k}}',${{!on}})">${{k}}</button>`;}}).join("")}}
  <button onclick="vote('${{i.id}}','impression',true)">mark seen</button></div></div>`).join("");}}
async function refresh(){{try{{render(await api(`/api/lobbies/${{encodeURIComponent(id)}}/feed`));}}catch(e){{document.getElementById("feed").innerHTML=`<div class="card warn">${{esc(e.message)}} — <a href="/dev">sign in</a></div>`;}}}}
async function start(){{
 const me=await fetch("/api/me");if(!me.ok){{document.getElementById("who").innerHTML='Not signed in — <a href="/dev">dev sign-in</a> or <a href="/auth/x/login">Sign in with X</a>';return;}}
 document.getElementById("who").textContent="Viewing as "+(await me.json()).user_id;
 const p=await api(`/api/lobbies/${{encodeURIComponent(id)}}`);document.getElementById("title").textContent=p.name;
 await refresh();
 const es=new EventSource(`/api/lobbies/${{encodeURIComponent(id)}}/events`);let t=null;
 es.onmessage=null;["interaction","contribution","member","policy","withdrawal","resync"].forEach(k=>es.addEventListener(k,()=>{{clearTimeout(t);t=setTimeout(refresh,150);}}));
}}
start();
</script></body></html>"#))
}
