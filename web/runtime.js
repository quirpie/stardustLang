// web/runtime.js — Runtime compartido del renderer de widgets de la StardustVM.
//
// Se inyecta *inline* en las páginas que embeben la VM (marcador __RUNTIME__).
// Requiere que `Engine` (el glue de wasm-bindgen) esté en el scope. No toca DOM
// global: cada runtime pinta dentro del `rootEl` que se le pasa, de modo que una
// misma página puede hospedar VARIOS (p. ej. el playground: biblioteca + runner).
//
// La lógica de widgets vive aquí una sola vez; el árbol ya viene resuelto del
// núcleo Rust (`engine.render()`), así que esto es un pintor tonto + el puente
// asíncrono (eventos, red, espejo del VFS).

function b64ToBytes(b64){ const bin=atob(b64); const b=new Uint8Array(bin.length); for(let i=0;i<bin.length;i++) b[i]=bin.charCodeAt(i); return b; }
function bytesToB64(bytes){ let bin=""; const ch=0x8000; for(let i=0;i<bytes.length;i+=ch) bin+=String.fromCharCode.apply(null, bytes.subarray(i,i+ch)); return btoa(bin); }

// Sustituye los marcadores {"$host":..} del payload por el dato de la interacción.
function substitute(j, host){
  if (Array.isArray(j)) return j.map(x=>substitute(x, host));
  if (j && typeof j==="object"){
    const keys=Object.keys(j);
    if (keys.length===1 && keys[0]==="$host"){
      const name=j.$host;
      if (name==="bytes") return { "$type":"bytes", "v": bytesToB64(host.bytes || new Uint8Array()) };
      return host[name] !== undefined ? host[name] : null;
    }
    const o={}; for (const k of keys) o[k]=substitute(j[k], host); return o;
  }
  return j;
}

// Detección de tipo para el widget `image` (previsualiza bytes).
function sniffMime(bytes, name){
  if (bytes.length>=4 && bytes[0]===0x89 && bytes[1]===0x50) return "image/png";
  if (bytes.length>=2 && bytes[0]===0xFF && bytes[1]===0xD8) return "image/jpeg";
  if (bytes.length>=3 && bytes[0]===0x47 && bytes[1]===0x49 && bytes[2]===0x46) return "image/gif";
  if (bytes.length>=12 && bytes[0]===0x52 && bytes[1]===0x49 && bytes[8]===0x57) return "image/webp";
  if (bytes.length>=4 && bytes[0]===0x25 && bytes[1]===0x50 && bytes[2]===0x44 && bytes[3]===0x46) return "application/pdf";
  const ext=(name.split(".").pop()||"").toLowerCase();
  const map={ svg:"image/svg+xml", png:"image/png", jpg:"image/jpeg", jpeg:"image/jpeg", gif:"image/gif",
    webp:"image/webp", pdf:"application/pdf", txt:"text/plain", md:"text/markdown", json:"application/json",
    csv:"text/csv", html:"text/html", css:"text/css", js:"text/javascript", xml:"application/xml" };
  return map[ext] || null;
}
function tryUtf8(bytes){ try { return new TextDecoder("utf-8",{fatal:true}).decode(bytes); } catch(_){ return null; } }
function hexDump(bytes){ let out=""; const n=Math.min(bytes.length,512);
  for (let i=0;i<n;i+=16){ const row=bytes.subarray(i,i+16);
    const hex=[...row].map(x=>x.toString(16).padStart(2,"0")).join(" ");
    const asc=[...row].map(x=>x>=32&&x<127?String.fromCharCode(x):".").join("");
    out+=i.toString(16).padStart(6,"0")+"  "+hex.padEnd(48)+"  "+asc+"\n"; }
  if (bytes.length>n) out+="… ("+(bytes.length-n)+" bytes más)\n"; return out || "(vacío)"; }

// Disco persistente de un Engine: espeja su VFS en una base IndexedDB propia
// (`name`, p. ej. "stardust-run:<app>"; un store "files" ruta → bytes). Devuelve
//   mount(engine)  : async, vuelca el disco al VFS (vfs_put) y re-ejecuta los
//                    on_start (remount) para que un FILE_LIST vea lo montado;
//                    devuelve la traza de ese re-boot.
//   mirror(engine) : async, aplica take_dirty() — escribe o borra solo lo cambiado.
// La asincronía de IndexedDB vive solo aquí: el intérprete sigue síncrono. Si
// IndexedDB no está disponible (modo privado, file:// restringido) degrada a un
// disco volátil: la app funciona, pero no persiste.
function openStardustDisk(name){
  const ready = new Promise((resolve)=>{
    try {
      const req=indexedDB.open(name, 1);
      req.onupgradeneeded=()=>req.result.createObjectStore("files");
      req.onsuccess=()=>resolve(req.result);
      req.onerror=()=>resolve(null);
    } catch(_){ resolve(null); }
  });
  const done=(req)=>new Promise((res,rej)=>{ req.onsuccess=()=>res(req.result); req.onerror=()=>rej(req.error); });

  async function mount(engine){
    const db=await ready;
    if (db){
      const tx=db.transaction("files","readonly"), store=tx.objectStore("files");
      const [keys, vals]=await Promise.all([done(store.getAllKeys()), done(store.getAll())]);
      keys.forEach((k,i)=>engine.vfs_put(String(k), new Uint8Array(vals[i])));
    }
    return JSON.parse(engine.remount()).trace || [];
  }

  async function mirror(engine){
    const dirty=JSON.parse(engine.take_dirty());
    const db=await ready;
    if (!db || !dirty.length) return;
    const tx=db.transaction("files","readwrite"), store=tx.objectStore("files");
    for (const d of dirty){
      if (d.deleted) store.delete(d.path);
      else { const bytes=engine.vfs_get(d.path); if (bytes) store.put(bytes, d.path); }
    }
    await new Promise((res,rej)=>{ tx.oncomplete=res; tx.onerror=()=>rej(tx.error); tx.onabort=()=>rej(tx.error); });
  }

  async function close(){ const db=await ready; if (db) db.close(); }

  return { name, persistent: ready.then(db=>!!db), mount, mirror, close };
}

// Borra por completo el disco persistente `name` (p. ej. al eliminar una app).
function dropStardustDisk(name){
  return new Promise((res)=>{ try { const r=indexedDB.deleteDatabase(name); r.onsuccess=r.onerror=r.onblocked=()=>res(); } catch(_){ res(); } });
}

// Crea un runtime para UN programa. `opts`:
//   source  : texto JSON del programa StardustLang
//   rootEl  : contenedor DOM donde pintar la vista
//   onTrace : (line)=>void            (opcional) para el bus de mensajes
//   mirror  : async (engine)=>void    (opcional) para espejar el VFS tras cada cambio
// Devuelve { engine, start(), rerender(), fire(event,host), pumpNet() }.
function createStardustRuntime(opts){
  const { source, rootEl } = opts;
  const onTrace = opts.onTrace || (()=>{});
  const mirror = opts.mirror || (async()=>{});
  const engine = new Engine(source);

  // El pintor: un `case` por widget, cero lógica de negocio.
  function paint(node){
    switch (node.kind){
      case "label": { const d=document.createElement("div"); d.className="w-label"; d.textContent=node.text; return d; }
      case "button": { const b=document.createElement("button"); b.type="button"; b.className="w-button";
        b.textContent=node.label; b.addEventListener("click",()=>fire(node.event)); return b; }
      case "row": { const d=document.createElement("div"); d.className="w-row"; (node.children||[]).forEach(c=>d.appendChild(paint(c))); return d; }
      case "column": { const d=document.createElement("div"); d.className="w-col"; (node.children||[]).forEach(c=>d.appendChild(paint(c))); return d; }
      case "grid": { const d=document.createElement("div"); d.className="w-grid";
        d.style.gridTemplateColumns="repeat("+node.columns+",1fr)"; (node.children||[]).forEach(c=>d.appendChild(paint(c))); return d; }
      case "list": { const d=document.createElement("div"); d.className="w-list"; (node.children||[]).forEach(c=>d.appendChild(paint(c))); return d; }
      case "input": { const form=document.createElement("form"); form.className="w-input";
        const inp=document.createElement("input"); inp.type="text"; inp.value=node.value||""; inp.placeholder=node.placeholder||""; form.appendChild(inp);
        if (node.label){ const btn=document.createElement("button"); btn.type="submit"; btn.className="w-button"; btn.textContent=node.label; form.appendChild(btn); }
        form.addEventListener("submit",e=>{ e.preventDefault(); const v=inp.value.trim(); if(!v) return; fire(node.event,{input:v}); });
        return form; }
      case "image": { const wrap=document.createElement("div"); wrap.className="w-image";
        if (!node.src_b64){ wrap.innerHTML='<div class="w-empty">sin selección</div>'; return wrap; }
        const bytes=b64ToBytes(node.src_b64); const mime=sniffMime(bytes, node.alt||"");
        if (mime && mime.startsWith("image/")){ const img=document.createElement("img"); img.alt=node.alt||""; img.src="data:"+mime+";base64,"+node.src_b64; wrap.appendChild(img); }
        else if (mime==="application/pdf"){ const f=document.createElement("iframe"); f.src="data:application/pdf;base64,"+node.src_b64; wrap.appendChild(f); }
        else { const text=tryUtf8(bytes); const pre=document.createElement("pre"); pre.textContent = text!==null ? text : hexDump(bytes); wrap.appendChild(pre); }
        return wrap; }
      case "filedrop": { const label=document.createElement("label"); label.className="w-drop";
        const strong=document.createElement("strong"); strong.textContent=node.label||"Arrastra archivos aquí"; label.appendChild(strong);
        label.appendChild(document.createTextNode("imágenes · PDF · texto · lo que sea"));
        const inp=document.createElement("input"); inp.type="file"; inp.multiple=true; inp.style.display="none"; label.appendChild(inp);
        const ingest=async(files)=>{ for (const f of files){ const bytes=new Uint8Array(await f.arrayBuffer()); await fire(node.event,{ name:f.name, bytes }); } };
        inp.addEventListener("change",e=>{ if(e.target.files.length) ingest(e.target.files); inp.value=""; });
        ["dragenter","dragover"].forEach(ev=>label.addEventListener(ev,e=>{ e.preventDefault(); label.classList.add("over"); }));
        ["dragleave","drop"].forEach(ev=>label.addEventListener(ev,e=>{ e.preventDefault(); label.classList.remove("over"); }));
        label.addEventListener("drop",e=>{ if(e.dataTransfer.files.length) ingest(e.dataTransfer.files); });
        return label; }
      case "textarea": { const wrap=document.createElement("div"); wrap.className="w-textarea";
        const ta=document.createElement("textarea"); ta.value=node.value||""; ta.placeholder=node.placeholder||""; ta.spellcheck=false;
        const bar=document.createElement("div"); bar.className="w-textarea-bar";
        const btn=document.createElement("button"); btn.type="button"; btn.className="w-button"; btn.textContent=node.label||"Guardar";
        btn.addEventListener("click",()=>fire(node.event,{input:ta.value}));
        ta.addEventListener("keydown",e=>{ if((e.ctrlKey||e.metaKey)&&e.key==="Enter"){ e.preventDefault(); fire(node.event,{input:ta.value}); } });
        bar.appendChild(btn); wrap.append(ta, bar); return wrap; }
      case "html": { const d=document.createElement("div"); d.className="w-html"; d.innerHTML=node.html||""; return d; }
      default: { const d=document.createElement("div"); d.className="w-empty"; d.textContent="widget desconocido: "+node.kind; return d; }
    }
  }

  function rerender(){
    const tree=JSON.parse(engine.render());
    rootEl.replaceChildren(tree ? paint(tree)
      : Object.assign(document.createElement("div"),{ className:"w-empty", textContent:"(este programa no declara vista)" }));
  }

  // Inyecta un evento como mensaje, espeja el VFS, re-renderiza y bombea la red.
  async function fire(event, host){
    const payload=substitute(event.send, host || {});
    const res=JSON.parse(engine.event(event.to || "", JSON.stringify(payload)));
    (res.trace||[]).forEach(onTrace);
    await mirror(engine);
    rerender();
    await pumpNet();
  }

  // Bombeo de red: ejecuta lo que la VM encoló (NET_FETCH y SEND a actor://,
  // ambos como POST/fetch), entrega la respuesta con deliver() y re-renderiza. La
  // asincronía de la red vive SOLO aquí; el intérprete WASM nunca se bloqueó.
  async function pumpNet(){
    const pending=JSON.parse(engine.take_outbound());
    for (const req of pending){
      let status=0, headers={}, body=new Uint8Array(), error=null;
      try {
        const init={ method:req.method, headers:req.headers };
        if (!/^(GET|HEAD)$/.test(req.method)) init.body=b64ToBytes(req.body_b64);
        const r=await fetch(req.url, init);
        status=r.status; r.headers.forEach((v,k)=>{ headers[k]=v; });
        body=new Uint8Array(await r.arrayBuffer());
      } catch (e){ error=String(e && e.message ? e.message : e); }
      const result=JSON.parse(engine.deliver(req.corr, status, JSON.stringify(headers), body, error));
      (result.trace||[]).forEach(onTrace);
      await mirror(engine);
      rerender();
      await pumpNet(); // una respuesta puede encolar nuevas peticiones
    }
  }

  async function start(){ rerender(); await pumpNet(); }

  return { engine, start, rerender, fire, pumpNet };
}
