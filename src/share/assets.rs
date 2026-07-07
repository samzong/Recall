pub(crate) const HEADERS: &str = "/*\n  X-Robots-Tag: noindex, nofollow\n  X-Frame-Options: DENY\n  X-Content-Type-Options: nosniff\n  Referrer-Policy: no-referrer\n  Cache-Control: no-store\n";
pub(crate) const ROBOTS: &str = "User-agent: *\nDisallow: /\n";
pub(crate) const SESSION_PAGE_CSS: &str = include_str!("assets/session.css");
pub(crate) const CHEVRON_SVG: &str = "<svg class=\"chev\" viewBox=\"0 0 16 16\" fill=\"none\" aria-hidden=\"true\"><path d=\"M6 4l4 4-4 4\" stroke=\"currentColor\" stroke-width=\"1.5\" stroke-linecap=\"round\" stroke-linejoin=\"round\"/></svg>";

pub(crate) const TOC_NAV_SCRIPT: &str = r#"<script>
(function(){
  function setup(){
    var sections = Array.prototype.slice.call(document.querySelectorAll('.turn.user'));
    var ticks = Array.prototype.slice.call(document.querySelectorAll('.tick'));
    if(!sections.length || !ticks.length) return;
    var tickFor = {};
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
  if(document.readyState === 'loading') document.addEventListener('DOMContentLoaded', setup);
  else setup();
})();
</script>"#;
