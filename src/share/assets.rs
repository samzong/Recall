pub(crate) const HEADERS: &str = "/*\n  X-Robots-Tag: noindex, nofollow\n  X-Frame-Options: DENY\n  X-Content-Type-Options: nosniff\n  Referrer-Policy: no-referrer\n  Cache-Control: no-store\n";
pub(crate) const ROBOTS: &str = "User-agent: *\nDisallow: /\n";
pub(crate) const SESSION_PAGE_CSS: &str = include_str!("assets/session.css");
pub(crate) const CHEVRON_SVG: &str = "<svg class=\"chev\" viewBox=\"0 0 16 16\" fill=\"none\" aria-hidden=\"true\"><path d=\"M6 4l4 4-4 4\" stroke=\"currentColor\" stroke-width=\"1.5\" stroke-linecap=\"round\" stroke-linejoin=\"round\"/></svg>";

pub(crate) const SESSION_PAGE_SCRIPT: &str = r#"<script>
(function(){
  function setupDetails(scopeEl, detailsToggle){
    if(!detailsToggle) return function(){};
    function getDetails(){
      return scopeEl ? Array.prototype.slice.call(scopeEl.querySelectorAll('details')) : [];
    }
    function syncDetails(){
      var details = getDetails();
      if(!details.length){ detailsToggle.hidden = true; return; }
      detailsToggle.hidden = false;
      var expanded = details.every(function(d){ return d.open; });
      detailsToggle.textContent = expanded ? 'Collapse all' : 'Expand all';
      detailsToggle.setAttribute('aria-expanded', expanded ? 'true' : 'false');
    }
    return {
      setScope: function(el){
        scopeEl = el;
        syncDetails();
      },
      toggleAll: function(){
        var details = getDetails();
        if(!details.length) return;
        var expanded = !details.every(function(d){ return d.open; });
        details.forEach(function(d){ d.open = expanded; });
        syncDetails();
      },
      sync: syncDetails
    };
  }

  function setupSingle(){
    var detailsToggle = document.querySelector('.details-toggle');
    var details = Array.prototype.slice.call(document.querySelectorAll('details'));
    if(detailsToggle && details.length){
      detailsToggle.hidden = false;
      function allOpen(){ return details.every(function(detail){ return detail.open; }); }
      function syncDetailsToggle(){
        var expanded = allOpen();
        detailsToggle.textContent = expanded ? 'Collapse all' : 'Expand all';
        detailsToggle.setAttribute('aria-expanded', expanded ? 'true' : 'false');
      }
      detailsToggle.addEventListener('click', function(){
        var expanded = !allOpen();
        details.forEach(function(detail){ detail.open = expanded; });
        syncDetailsToggle();
      });
      details.forEach(function(detail){ detail.addEventListener('toggle', syncDetailsToggle); });
      syncDetailsToggle();
    }
    var sections = Array.prototype.slice.call(document.querySelectorAll('.turn.user'));
    var ticks = Array.prototype.slice.call(document.querySelectorAll('.tick'));
    if(!sections.length || !ticks.length) return;
    var tickFor = Object.create(null);
    ticks.forEach(function(t){ var h=t.getAttribute('href'); if(h) tickFor[h.slice(1)]=t; });
    var activeId = sections[0].id, offset = 150;
    function compute(){
      var cur = sections[0].id;
      for(var i=0;i<sections.length;i++){
        if(sections[i].getBoundingClientRect().top - offset <= 0) cur = sections[i].id; else break;
      }
      activeId = cur;
      ticks.forEach(function(t){ t.classList.remove('active'); });
      if(tickFor[cur]) tickFor[cur].classList.add('active');
    }
    var ticking = false;
    window.addEventListener('scroll', function(){
      if(ticking) return; ticking = true;
      requestAnimationFrame(function(){ compute(); ticking = false; });
    }, { passive:true });
    compute();
    function go(dir){
      var idx = sections.map(function(s){return s.id;}).indexOf(activeId);
      var n = Math.max(0, Math.min(sections.length-1, idx+dir));
      var top = sections[n].getBoundingClientRect().top + window.scrollY - 120;
      window.scrollTo({ top: top, behavior: 'smooth' });
    }
    var up = document.querySelector('.toc-up'), down = document.querySelector('.toc-down');
    if(up) up.addEventListener('click', function(){ go(-1); });
    if(down) down.addEventListener('click', function(){ go(1); });
  }

  function setupBundle(panels){
    document.documentElement.classList.add('js-bundle');
    var panelById = Object.create(null);
    panels.forEach(function(p){
      var id = p.getAttribute('data-panel-id');
      if(id) panelById[id] = p;
    });
    function hasPanel(id){
      return !!(id && Object.prototype.hasOwnProperty.call(panelById, id) && panelById[id]);
    }
    var tocs = Array.prototype.slice.call(document.querySelectorAll('.user-toc[data-toc-for]'));
    var treeLinks = Array.prototype.slice.call(document.querySelectorAll('[data-panel-target]'));
    var detailsToggle = document.querySelector('.details-toggle');
    var detailsCtrl = setupDetails(null, detailsToggle);
    if(detailsToggle){
      detailsToggle.addEventListener('click', function(){ detailsCtrl.toggleAll(); });
    }
    panels.forEach(function(panel){
      Array.prototype.slice.call(panel.querySelectorAll('details')).forEach(function(d){
        d.addEventListener('toggle', function(){
          if(panel.classList.contains('is-active')) detailsCtrl.sync();
        });
      });
    });

    var activeSections = [];
    var activeTicks = [];
    var tickFor = Object.create(null);
    var activeTurnId = '';
    var offset = 150;

    function computeToc(){
      if(!activeSections.length || !activeTicks.length) return;
      var cur = activeSections[0].id;
      for(var i=0;i<activeSections.length;i++){
        if(activeSections[i].getBoundingClientRect().top - offset <= 0) cur = activeSections[i].id; else break;
      }
      activeTurnId = cur;
      activeTicks.forEach(function(t){ t.classList.remove('active'); });
      if(tickFor[cur]) tickFor[cur].classList.add('active');
    }

    var ticking = false;
    window.addEventListener('scroll', function(){
      if(ticking) return; ticking = true;
      requestAnimationFrame(function(){ computeToc(); ticking = false; });
    }, { passive:true });

    function bindTocNav(toc){
      if(!toc || toc.getAttribute('data-bound') === '1') return;
      toc.setAttribute('data-bound', '1');
      function go(dir){
        if(!activeSections.length) return;
        var idx = activeSections.map(function(s){ return s.id; }).indexOf(activeTurnId);
        if(idx < 0) idx = 0;
        var n = Math.max(0, Math.min(activeSections.length-1, idx+dir));
        var top = activeSections[n].getBoundingClientRect().top + window.scrollY - 120;
        window.scrollTo({ top: top, behavior: 'smooth' });
      }
      var up = toc.querySelector('.toc-up'), down = toc.querySelector('.toc-down');
      if(up) up.addEventListener('click', function(){ go(-1); });
      if(down) down.addEventListener('click', function(){ go(1); });
    }

    function updateHeaderMeta(panel){
      var labelEl = document.querySelector('[data-active-session-label]');
      if(labelEl){
        labelEl.textContent = panel.getAttribute('data-panel-meta') || '';
      }
      var mobileTitleEl = document.querySelector('[data-active-tree-title]');
      if(mobileTitleEl){
        mobileTitleEl.textContent = panel.getAttribute('data-panel-title') || '';
      }
      var tagsEl = document.querySelector('[data-active-meta-tags]');
      if(tagsEl){
        while(tagsEl.firstChild) tagsEl.removeChild(tagsEl.firstChild);
        var models = panel.getAttribute('data-panel-models') || '';
        var thinking = panel.getAttribute('data-panel-thinking') || '';
        function addTag(kind, val){
          if(!val) return;
          var span = document.createElement('span');
          span.className = 'meta-tag';
          span.appendChild(document.createTextNode(kind + ' '));
          var b = document.createElement('b');
          b.textContent = val;
          span.appendChild(b);
          tagsEl.appendChild(span);
        }
        addTag('model', models);
        addTag('thinking', thinking);
        tagsEl.hidden = !tagsEl.firstChild;
      }
    }

    function activatePanel(panelId, updateUrl, preserveHash){
      var panel = hasPanel(panelId) ? panelById[panelId] : panels[0];
      panelId = panel.getAttribute('data-panel-id');
      panels.forEach(function(p){
        p.classList.toggle('is-active', p === panel);
      });
      var activeToc = null;
      tocs.forEach(function(t){
        var match = t.getAttribute('data-toc-for') === panelId;
        t.classList.toggle('is-active', match);
        if(match) activeToc = t;
      });
      treeLinks.forEach(function(link){
        if(link.classList.contains('tree-item')){
          link.classList.toggle('is-active', link.getAttribute('data-panel-target') === panelId);
        }
      });
      var mobileSwitcher = document.querySelector('.mobile-tree-switcher');
      if(mobileSwitcher && !preserveHash) mobileSwitcher.open = false;

      updateHeaderMeta(panel);
      detailsCtrl.setScope(panel);

      activeSections = Array.prototype.slice.call(panel.querySelectorAll('.turn.user'));
      activeTicks = activeToc ? Array.prototype.slice.call(activeToc.querySelectorAll('.tick')) : [];
      tickFor = Object.create(null);
      activeTicks.forEach(function(t){ var h=t.getAttribute('href'); if(h) tickFor[h.slice(1)]=t; });
      bindTocNav(activeToc);
      computeToc();

      if(updateUrl && window.history && window.history.replaceState){
        try {
          var nextUrl = new URL(window.location.href);
          nextUrl.searchParams.set('s', panelId);
          if(!preserveHash) nextUrl.hash = '';
          window.history.replaceState(null, '', nextUrl.toString());
        } catch(e){}
      }
    }

    treeLinks.forEach(function(link){
      link.addEventListener('click', function(e){
        if(e.metaKey || e.ctrlKey || e.shiftKey || e.altKey || e.button !== 0) return;
        var target = link.getAttribute('data-panel-target');
        if(!hasPanel(target)) return;
        e.preventDefault();
        activatePanel(target, true, false);
        window.scrollTo({ top: 0 });
      });
    });

    var params = new URLSearchParams(window.location.search);
    var requested = params.get('s');
    var initialId = hasPanel(requested) ? requested : null;
    if(!initialId && window.location.hash){
      var rawHash = window.location.hash.slice(1);
      var dash = rawHash.indexOf('-user-');
      if(dash > 0){
        var candidate = rawHash.slice(0, dash);
        if(hasPanel(candidate)) initialId = candidate;
      }
    }
    if(!initialId) initialId = panels[0].getAttribute('data-panel-id');
    activatePanel(initialId, true, true);
    if(window.location.hash){
      var anchor = document.getElementById(window.location.hash.slice(1));
      if(anchor) anchor.scrollIntoView();
    }
  }

  function setup(){
    var panels = Array.prototype.slice.call(document.querySelectorAll('.session-panel'));
    if(panels.length){
      setupBundle(panels);
    } else {
      setupSingle();
    }
  }
  if(document.readyState === 'loading') document.addEventListener('DOMContentLoaded', setup);
  else setup();
})();
</script>"#;
