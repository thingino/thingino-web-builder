  const $=id=>document.getElementById(id);
  const API = window.API_BASE || '';
  const esc=s=>String(s??'').replace(/[&<>"']/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));
  const LS_KEY='thingino_builder_uid', MY_KEY='thingino_my_build';
  const getUid=()=>localStorage.getItem(LS_KEY)||'';
  const setUid=u=>{ if(u) localStorage.setItem(LS_KEY,u); };
  let myId=localStorage.getItem(MY_KEY)||null;
  const setMy=id=>{ myId=id; if(id) localStorage.setItem(MY_KEY,id); else localStorage.removeItem(MY_KEY); };

  let allowed=new Set(), maxConc=6, avgSecs=null, curCommit=null, you=null, youAt=0;
  const ACTIVE=new Set(['queued','running','cancelling']);

  const fmt=s=>{ s=Math.max(0,Math.floor(s)); return `${Math.floor(s/60)}:${String(s%60).padStart(2,'0')}`; };
  const mins=s=> s==null?'—':`~${Math.max(1,Math.round(s/60))} min`;
  const spin=()=>'<span class="spinner-border spinner-border-sm text-warning me-2"></span>';

  async function api(path, opts={}) {
    opts.headers = Object.assign({'X-Builder-Uid':getUid()}, opts.headers||{});
    let r, data=null;
    try { r = await fetch(API + path, opts); } catch { return {ok:false,status:0,data:null}; }
    try { data = await r.json(); } catch {}
    if (data && data.uid) setUid(data.uid);
    return {ok:r.ok, status:r.status, data};
  }

  function validate(){
    const v=$('board').value.trim(); $('go').disabled=!allowed.has(v);
    const h=$('hint');
    if(v && !allowed.has(v)){ h.textContent='Not a known defconfig.'; h.className='form-text text-danger'; }
    else { h.textContent=allowed.size?`${allowed.size} camera profiles available.`:''; h.className='form-text muted'; }
  }
  async function loadBoards(){
    const {ok,data}=await api('/api/defconfigs');
    if(!ok||!Array.isArray(data)){ const h=$('hint'); h.textContent='Could not load camera list — is the broker up?'; h.className='form-text text-danger'; return; }
    data.sort(); allowed=new Set(data);
    $('boards').innerHTML=data.map(b=>`<option value="${esc(b)}">`).join('');
    validate();
  }

  function renderGlobal(d){
    curCommit=d.commit||null;
    $('stats').innerHTML=`<i class="bi bi-hdd-stack me-1"></i><b>${esc(d.running)}</b>/${esc(d.max_concurrent)} building &nbsp;·&nbsp; <b>${esc(d.queued)}</b> queued &nbsp;·&nbsp; typical build <b>${mins(d.avg_build_secs)}</b>`;
    const cb=$('commit-badge');
    if(curCommit){ cb.textContent='thingino @ '+curCommit.slice(0,7); cb.classList.remove('d-none'); } else cb.classList.add('d-none');
    if(d.version){ const v=$('version'); if(v) v.textContent=d.version; }
    const b=$('banner');
    if(d.builds_enabled===false){ b.innerHTML='<i class="bi bi-exclamation-triangle me-1"></i>Builds are temporarily disabled by the admin.'; b.classList.remove('d-none'); }
    else b.classList.add('d-none');
  }

  function renderYou(){
    const picker=$('picker'), mb=$('mybuild');
    if(!you){ mb.classList.add('d-none'); mb.innerHTML=''; picker.classList.remove('d-none'); return; }
    picker.classList.toggle('d-none', ACTIVE.has(you.state));
    mb.classList.remove('d-none');
    const live=(you.elapsed_secs||0)+(Date.now()-youAt)/1000;
    const meta=`<div class="small muted mt-2">defconfig <code>${esc(you.defconfig)}</code><br>build id <code>${esc(you.build_id)}</code>${you.deduped?'<br><span class="text-warning">reused an existing identical image</span>':''}</div>`;
    let h='';
    if(you.state==='queued')
      h=`<div class="alert alert-secondary mb-0">${spin()}<strong>Queued</strong> — position ${esc(you.position)}${meta}<div class="mt-2"><button class="btn btn-outline-secondary btn-sm" id="cancel">Cancel</button></div></div>`;
    else if(you.state==='running')
      h=`<div class="alert alert-secondary mb-0">${spin()}<strong>Building…</strong> ${fmt(live)}${meta}<div class="mt-2"><button class="btn btn-outline-secondary btn-sm" id="cancel">Cancel</button></div></div>`;
    else if(you.state==='cancelling')
      h=`<div class="alert alert-warning mb-0">${spin()}<strong>Cancelling…</strong><div class="small">stopping the build — this can take a moment</div>${meta}</div>`;
    else if(you.state==='done')
      h=`<div class="alert alert-success mb-0"><i class="bi bi-check-circle-fill me-1"></i><strong>Build ready</strong>${meta}
        <div class="mt-2 d-flex gap-2 flex-wrap"><a class="btn btn-thingino btn-sm" href="${esc(you.download_url)}" download><i class="bi bi-download me-1"></i>Download .bin</a>
        <button class="btn btn-outline-secondary btn-sm" id="again">Build another</button></div>
        <div class="small text-warning mt-2"><i class="bi bi-clock me-1"></i>Available for ~30 minutes, then removed.</div></div>`;
    else if(you.state==='failed')
      h=`<div class="alert alert-danger mb-0"><i class="bi bi-exclamation-triangle-fill me-1"></i><strong>Build failed</strong>${meta}<div class="mt-2"><button class="btn btn-outline-warning btn-sm" id="again">Try again</button></div></div>`;
    else
      h=`<div class="alert alert-secondary mb-0"><strong>${you.state==='expired'?'Download window expired':'Cancelled'}</strong>${meta}<div class="mt-2"><button class="btn btn-outline-secondary btn-sm" id="again">Build again</button></div></div>`;
    mb.innerHTML=h;
    const c=$('cancel'); if(c) c.onclick=cancelBuild;
    const a=$('again'); if(a) a.onclick=()=>{ setMy(null); you=null; renderYou(); $('board').focus(); };
  }

  async function refresh(){
    const {ok,data}=await api('/api/stats');
    if(ok&&data){ maxConc=data.max_concurrent||6; avgSecs=data.avg_build_secs; renderGlobal(data);
      if(!myId && data.you){ setMy(data.you.build_id); } }
    if(myId){
      const s=await api('/api/status/'+myId);
      if(s.status===404){ setMy(null); you=null; renderYou(); }
      else if(s.ok&&s.data&&s.data.state){ you=s.data; youAt=Date.now(); renderYou(); }
    } else { you=null; renderYou(); }
  }

  async function submit(){
    const defconfig=$('board').value.trim();
    if(!allowed.has(defconfig)) return;
    $('go').disabled=true;
    const {ok,status,data}=await api('/api/build',{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({defconfig})});
    if(!ok){ const h=$('hint'); h.textContent=(data&&data.error)||`Request failed (${status}).`; h.className='form-text text-danger'; $('go').disabled=false; return; }
    setMy(data.build_id);
    you={build_id:data.build_id, defconfig:data.defconfig, state:data.state||'queued', position:data.position||0, elapsed_secs:0, download_url:data.download_url, deduped:data.deduped};
    youAt=Date.now(); renderYou(); refresh();
  }

  async function cancelBuild(){
    if(!you) return;
    const b=$('cancel'); if(b){ b.disabled=true; b.textContent='Cancelling…'; }
    const {data}=await api(`/api/cancel/${you.build_id}`,{method:'POST'});
    if(data&&data.state){ you.state=(data.state==='cancelled')?'cancelled':'cancelling'; youAt=Date.now(); renderYou(); }
    refresh();
  }

  $('board').addEventListener('input',validate);
  $('board').addEventListener('keydown',e=>{ if(e.key==='Enter'&&!$('go').disabled) submit(); });
  $('go').addEventListener('click',submit);
  loadBoards(); refresh();
  setInterval(refresh, 5000);
  setInterval(()=>{ if(you&&you.state==='running') renderYou(); }, 1000);
