pub const INDEX_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>Deezco — Player</title>
<style>
*{box-sizing:border-box;margin:0;padding:0}
body{font-family:system-ui,-apple-system,Segoe UI,Roboto,sans-serif;background:#0f0f0f;color:#e8e8e8;min-height:100vh;display:flex;flex-direction:column}
header{padding:16px 20px;border-bottom:1px solid #222;display:flex;align-items:center;gap:12px}
header h1{font-size:18px;letter-spacing:.04em}
header span{opacity:.6;font-size:13px}
main{flex:1;display:grid;grid-template-columns:320px 1fr;gap:0;min-height:0}
@media(max-width:700px){main{grid-template-columns:1fr}}
#sidebar{border-right:1px solid #222;padding:16px;display:flex;flex-direction:column;gap:12px;background:#111}
#sidebar input, #sidebar button{width:100%;padding:10px 12px;border-radius:8px;border:1px solid #333;background:#1a1a1a;color:#eee;font-size:14px}
#sidebar button{cursor:pointer;background:#6c2cff;border-color:#6c2cff;font-weight:600}
#sidebar button:disabled{opacity:.5;cursor:not-allowed}
#sidebar button.secondary{background:#222;border-color:#333}
#playlistInfo{font-size:12px;opacity:.6}
#trackList{flex:1;overflow:auto;display:flex;flex-direction:column;gap:6px;max-height:50vh;scrollbar-width:thin;scrollbar-color:#6c2cff transparent}
#trackList::-webkit-scrollbar{width:8px}
#trackList::-webkit-scrollbar-track{background:transparent}
#trackList::-webkit-scrollbar-thumb{background:#333;border-radius:999px}
#trackList::-webkit-scrollbar-thumb:hover{background:#6c2cff}
.track{padding:8px 10px;border-radius:8px;background:#1a1a1a;border:1px solid #222;cursor:pointer;font-size:13px}
.track:hover{border-color:#444}
.track.active{border-color:#6c2cff;background:#1e1630}
#player{padding:20px;display:flex;flex-direction:column;gap:16px}
#nowPlaying{background:#1a1a1a;border:1px solid #222;border-radius:12px;padding:16px}
#nowPlaying h2{font-size:16px;margin-bottom:4px}
#nowPlaying p{font-size:13px;opacity:.7}
#controls{display:flex;gap:10px;align-items:center;flex-wrap:wrap}
#controls button{padding:10px 16px;border-radius:999px;border:1px solid #333;background:#222;color:#eee;cursor:pointer}
#controls button.primary{background:#6c2cff;border-color:#6c2cff;font-weight:600}
audio{width:100%;margin-top:8px}
.badge{font-size:11px;padding:2px 6px;border-radius:999px;background:#222;border:1px solid #333}
.status{font-size:12px;opacity:.7}
code{font-size:12px;background:#222;padding:2px 6px;border-radius:4px}
</style>
</head>
<body>
<header>
  <h1>Deezco</h1>
  <span>web player</span>
  <span id="health" class="badge" style="margin-left:auto">checking…</span>
</header>
<main>
  <div id="sidebar">
    <input id="playlistId" placeholder="Playlist ID or URL (e.g. 908622995)">
    <button id="loadBtn">Load playlist</button>
    <button id="nextBtn" class="secondary" disabled>Next track (via API)</button>
    <div id="playlistInfo"></div>
    <div id="trackList"></div>
    <div class="status">
      API endpoints:<br>
      <code>GET /playlists/:id</code><br>
      <code>GET /playlists/:id/next</code><br>
      <code>GET /tracks/:id</code>
    </div>
  </div>
  <div id="player">
    <div id="nowPlaying">
      <h2 id="title">No track</h2>
      <p id="artist">Load a playlist to start</p>
      <audio id="audio" controls preload="none"></audio>
      <div id="meta" class="status" style="margin-top:8px"></div>
    </div>
    <div id="controls">
      <button id="playBtn" class="primary">Play</button>
      <button id="skipBtn">Skip →</button>
      <button id="autoBtn" class="secondary">Autoplay: OFF</button>
      <span id="status" class="status"></span>
    </div>
    <div class="status">
      This player is a thin control-plane on top of existing <code>serve</code>.
      Icecast streaming (<code>deezco stream</code>) stays separate — future web control plane will add <code>POST /api/icecast/*</code> without touching these 3 routes.
    </div>
  </div>
</main>
<script>
const $ = s => document.querySelector(s);
const playlistIdEl = $('#playlistId');
const loadBtn = $('#loadBtn');
const nextBtn = $('#nextBtn');
const trackListEl = $('#trackList');
const playlistInfoEl = $('#playlistInfo');
const titleEl = $('#title');
const artistEl = $('#artist');
const metaEl = $('#meta');
const audioEl = $('#audio');
const statusEl = $('#status');
const healthEl = $('#health');
const playBtn = $('#playBtn');
const skipBtn = $('#skipBtn');
const autoBtn = $('#autoBtn');

let currentPlaylist = null;
let tracksCache = [];
let autoplay = false;

function extractId(input){
  input=(input||'').trim();
  if(!input) return '';
  if(input.includes('deezer.com')){
    const parts=input.replace(/\/$/,'').split('/');
    return parts[parts.length-1].split(/[?#]/)[0];
  }
  return input.trim();
}
function setStatus(m){ statusEl.textContent=m; }
async function checkHealth(){
  try{
    const r=await fetch('/api/health');
    const j=await r.json();
    healthEl.textContent='ok · '+j.status;
    healthEl.style.borderColor='#2a6';
  }catch(e){ healthEl.textContent='offline'; }
}
checkHealth();

async function loadPlaylist(){
  const id=extractId(playlistIdEl.value);
  if(!id){ setStatus('Enter playlist ID'); return; }
  currentPlaylist=id;
  loadBtn.disabled=true; setStatus('Loading playlist '+id+'…');
  try{
    const r=await fetch('/playlists/'+encodeURIComponent(id));
    if(!r.ok) throw new Error('HTTP '+r.status);
    const j=await r.json();
    tracksCache=j.tracks||[];
    playlistInfoEl.textContent=j.count+' tracks · ID '+j.id;
    trackListEl.innerHTML='';
    tracksCache.forEach(t=>{
      const div=document.createElement('div');
      div.className='track';
      div.dataset.id=t.id;
      div.innerHTML=`<b>${t.artist}</b> — ${t.title} <span class="badge">${Math.floor(t.duration/60)}:${String(t.duration%60).padStart(2,'0')}</span>`;
      div.onclick=()=> playTrack(t);
      trackListEl.appendChild(div);
    });
    nextBtn.disabled=false;
    setStatus('Loaded '+tracksCache.length+' tracks. Click a track or Next.');
    if(tracksCache[0]) playTrack(tracksCache[0]);
  }catch(e){ setStatus('Load failed: '+e.message); }
  finally{ loadBtn.disabled=false; }
}
async function fetchNext(){
  if(!currentPlaylist){ setStatus('Load a playlist first'); return; }
  setStatus('Fetching next…');
  try{
    const r=await fetch('/playlists/'+encodeURIComponent(currentPlaylist)+'/next');
    if(!r.ok) throw new Error((await r.json()).error||r.status);
    const t=await r.json();
    playTrack(t);
    setStatus('Next: '+t.artist+' — '+t.title);
  }catch(e){ setStatus('Next failed: '+e.message); }
}
function playTrack(t){
  document.querySelectorAll('.track').forEach(el=> el.classList.toggle('active', el.dataset.id===String(t.id)));
  titleEl.textContent=t.title||'Unknown';
  artistEl.textContent=t.artist||'';
  metaEl.textContent='ID '+t.id+' · '+t.duration+'s · '+t.url;
  audioEl.src=t.url;
  audioEl.play().catch(()=> setStatus('Click Play to start audio'));
}
loadBtn.onclick=loadPlaylist;
nextBtn.onclick=fetchNext;
skipBtn.onclick=fetchNext;
playBtn.onclick=()=> audioEl.play();
autoBtn.onclick=()=>{
  autoplay=!autoplay;
  autoBtn.textContent='Autoplay: '+(autoplay?'ON':'OFF');
  autoBtn.style.background=autoplay?'#6c2cff':'#222';
};
audioEl.addEventListener('ended', ()=>{ if(autoplay) fetchNext(); });
playlistIdEl.addEventListener('keydown', e=>{ if(e.key==='Enter') loadPlaylist(); });
// allow ?playlist=908622995 deep link
const qp=new URLSearchParams(location.search);
if(qp.get('playlist')){ playlistIdEl.value=qp.get('playlist'); loadPlaylist(); }
</script>
</body>
</html>
"#;
