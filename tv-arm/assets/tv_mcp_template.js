
import { readFileSync } from 'node:fs';
import { evaluate, evaluateAsync } from '{tv_mcp_root}/src/connection.js';

const payloadsJson = readFileSync('{payloads_path}', 'utf8');
const payloads = JSON.parse(payloadsJson);

const ctx = await evaluate(`
  (function() {
    var api = window.TradingViewApi._activeChartWidgetWV.value();
    var ms = api._chartWidget.model().mainSeries();
    var info = ms.symbolInfo();
    return {
      pro_name: info.pro_name,
      currency: info.currency_code,
      resolution: api.resolution(),
      layout_id: (window.location.pathname.match(/\\/chart\\/([^\\/]+)\\//) || [])[1] || null,
    };
  })()
`);

const results = [];
for (const item of payloads) {
  const expiration = new Date(Date.now() + 30 * 24 * 60 * 60 * 1000).toISOString();
  let condition;
  if (item.kind === 'pine_alertcondition') {
    const studyInfo = await evaluate(`
      (function() {
        var chart = window.TradingViewApi._activeChartWidgetWV.value();
        var name = ${JSON.stringify(item.indicator_name)};
        var sources = chart._chartWidget.model().dataSources();
        var titles = [];
        for (var i = 0; i < sources.length; i++) {
          var s = sources[i];
          var t = null;
          try { t = s.title && s.title(); } catch(e) {}
          var isStudy = false;
          try { isStudy = !!chart.getStudyById(s.id()); } catch(e) {}
          titles.push({ title: t == null ? null : String(t), isStudy: isStudy });
          try {
            var rawTitle = s.title && s.title();
            if (rawTitle == null) continue;
            var baseTitle = String(rawTitle).replace(/\\s*\\(.*$/, '');
            if (baseTitle !== name) continue;
            var id = s.id();
            var study = chart.getStudyById(id);
            if (!study) continue;
            var arr = study.getInputValues();
            var inputs = {};
            var pineId, pineVersion, pineFeatures;
            for (var j = 0; j < arr.length; j++) {
              var k = arr[j].id, v = arr[j].value;
              if (k === 'pineId') pineId = v;
              else if (k === 'pineVersion') pineVersion = v;
              else if (k === 'pineFeatures') pineFeatures = v;
              else if (/^in_\\d+$/.test(k)) inputs[k] = v;
            }
            // Map alertcondition TITLE -> live plot_N id from the study's
            // metaInfo, so tv-arm can bind by stable title and survive
            // plot reordering. metaInfo().plots carries {id, type}; the
            // alertcondition-typed entries' human title lives in
            // metaInfo().styles[id].title.
            var alertTitleToId = {};
            try {
              var mi = s.metaInfo();
              var mplots = (mi && mi.plots) || [];
              var mstyles = (mi && mi.styles) || {};
              for (var pi = 0; pi < mplots.length; pi++) {
                var pl = mplots[pi];
                if (!pl || pl.type !== 'alertcondition') continue;
                var st = mstyles[pl.id];
                var ttl = st && st.title;
                if (ttl != null) alertTitleToId[String(ttl)] = pl.id;
              }
            } catch(e) {}
            return { id: id, inputs: inputs, pineId: pineId, pineVersion: pineVersion, pineFeatures: pineFeatures, alertTitleToId: alertTitleToId };
          } catch(e) {}
        }
        return { __notFound: true, titles: titles };
      })()
    `);
    if (!studyInfo || studyInfo.__notFound) {
      const titles = (studyInfo && studyInfo.titles) || [];
      const summary = titles.map(function(t) {
        return (t.isStudy ? '[study] ' : '[other] ') + (t.title === null ? '<no-title>' : JSON.stringify(t.title));
      }).join(', ');
      results.push({
        name: item.name,
        error: 'study not found: ' + item.indicator_name + ' | data sources on active chart: [' + summary + ']',
      });
      continue;
    }
    // Resolve the alertcondition TITLE to the study's live plot_N id.
    // Fail loudly (no positional fallback) if the title isn't present —
    // a guessed index is exactly the silent err.code="general" failure
    // this resolver exists to eliminate.
    const titleToId = studyInfo.alertTitleToId || {};
    const resolvedCondId = titleToId[item.alert_cond_title];
    if (!resolvedCondId) {
      const known = Object.keys(titleToId);
      results.push({
        name: item.name,
        error: 'alertcondition title not found: ' + JSON.stringify(item.alert_cond_title)
          + ' on study ' + item.indicator_name
          + ' | available alertcondition titles: ['
          + known.map(function(t) { return JSON.stringify(t) + '=' + titleToId[t]; }).join(', ') + ']',
      });
      continue;
    }
    const orderedInputs = { pineFeatures: studyInfo.pineFeatures };
    const inNumKeys = Object.keys(studyInfo.inputs)
      .filter(function(k) { return /^in_\d+$/.test(k); })
      .sort(function(a, b) { return parseInt(a.slice(3), 10) - parseInt(b.slice(3), 10); });
    for (const k of inNumKeys) orderedInputs[k] = studyInfo.inputs[k];
    orderedInputs.__profile = false;
    const studySeries = {
      type: 'study',
      study: 'Script@tv-scripting-101',
      offsets_by_plot: {},
      inputs: orderedInputs,
      pine_id: studyInfo.pineId,
      pine_version: studyInfo.pineVersion,
    };
    condition = {
      type: 'alert_cond',
      frequency: item.frequency,
      alert_cond_id: resolvedCondId,
      series: [studySeries],
      resolution: ctx.resolution,
    };
  } else if (item.kind === 'vert_line_at') {
    // Synthetic vertical line — no drawing on the chart. Used for
    // calendar-derived bars where we have only a timestamp. The line
    // entry mirrors what stateForAlert() returns for a real
    // LineToolVertLine, with price1/price2 set to a neutral value
    // (TV's vertical-line evaluator ignores price; the handle is
    // placed at the centre of the visible price range anyway).
    const baseIso = new Date(item.base_time_epoch * 1000).toISOString();
    condition = {
      type: item.condition_type,
      frequency: item.frequency,
      series: [
        { type: 'barset' },
        {
          type: 'line',
          tool: 'LineToolVertLine',
          base_time: baseIso,
          offset1: 0,
          price1: 0,
          offset2: 1,
          price2: 0,
          extend_forward: false,
          extend_backward: false,
          layout_id: ctx.layout_id,
        },
      ],
      resolution: ctx.resolution,
    };
  } else if (item.kind === 'price_value') {
    // No drawing lookup — the alert is bound to a numeric price
    // level the script computed (pcl-exhausted at 80% of midpoint→TP).
    // Mirrors the create_alert payload TV's UI sends for "price
    // crossing value" alerts.
    condition = {
      type: item.condition_type,
      frequency: item.frequency,
      series: [
        { type: 'barset' },
        { type: 'value', value: item.value },
      ],
      resolution: ctx.resolution,
    };
  } else {
  const spec = await evaluate(`
    (function() {
      var api = window.TradingViewApi._activeChartWidgetWV.value();
      var sh = api.getShapeById(${JSON.stringify(item.drawing_id)});
      if (!sh) return null;
      try { return sh._source.stateForAlert(); } catch(e) { return { err: e.message }; }
    })()
  `);
  if (!spec || spec.err) {
    results.push({ name: item.name, error: 'stateForAlert failed: ' + (spec && spec.err || 'shape not found') });
    continue;
  }
  let lineEntry;
  if (item.tool === 'LineToolHorzLine') {
    const price = typeof spec.plots[0] === 'number' ? spec.plots[0] : spec.plots[0].price1;
    const resSec = parseInt(ctx.resolution, 10) * 60;
    const floor = Math.floor(Date.now() / 1000 / resSec) * resSec;
    lineEntry = {
      type: 'line',
      tool: 'LineToolHorzLine',
      base_time: new Date(floor * 1000).toISOString(),
      offset1: 0,
      price1: price,
      offset2: 1,
      price2: price,
      extend_forward: true,
      extend_backward: true,
      drawing_id: item.drawing_id,
      layout_id: ctx.layout_id,
    };
  } else {
    const plot = spec.plots[0];
    // For trendline alerts, we always need extend_forward:true — the line is
    // drawn over the H&S formation but the prep crossings (break-and-close,
    // retest) happen AFTER the second anchor. With extend_forward:false the
    // server's evaluator only considers the segment between anchors and
    // misses every real crossing. extend_backward stays as drawn (default
    // false) to avoid spurious fires from historical price action.
    var forceExtendForward = item.tool === 'LineToolTrendLine';
    lineEntry = {
      type: 'line',
      tool: item.tool,
      base_time: new Date(plot.timestamp * 1000).toISOString(),
      offset1: plot.offset1,
      price1: plot.price1,
      offset2: plot.offset2,
      price2: plot.price2,
      extend_forward: forceExtendForward || !!plot.extendForward,
      extend_backward: !!plot.extendBackward,
      drawing_id: item.drawing_id,
      layout_id: ctx.layout_id,
    };
  }
    condition = {
      type: item.condition_type,
      frequency: item.frequency,
      series: [
        { type: 'barset' },
        lineEntry,
      ],
      resolution: ctx.resolution,
    };
  }
  const body = {
    payload: {
      symbol: '=' + JSON.stringify({
        'currency-id': ctx.currency,
        session: 'regular',
        symbol: ctx.pro_name,
      }),
      resolution: ctx.resolution,
      message: item.message,
      sound_file: 'alert/fired',
      sound_duration: 0,
      popup: true,
      expiration: expiration,
      auto_deactivate: item.auto_deactivate !== false,
      email: false,
      sms_over_email: false,
      mobile_push: true,
      web_hook: 'https://trade-control-web-hook.msherborne.workers.dev',
      name: item.tv_name || null,
      conditions: [condition],
      active: true,
      ignore_warnings: true,
    },
  };
  const result = await evaluateAsync(`
    fetch('https://pricealerts.tradingview.com/create_alert', {
      method: 'POST',
      credentials: 'include',
      headers: { 'Content-Type': 'text/plain;charset=UTF-8' },
      body: ${JSON.stringify(JSON.stringify(body))},
    })
    .then(function(r) { return r.text().then(function(t) { return { status: r.status, body: t.slice(0, 2000) }; }); })
    .catch(function(e) { return { error: e.message }; })
  `);
  results.push({
    name: item.name,
    ...result,
    debug: {
      tool: item.tool,
      drawing_id: item.drawing_id,
      condition_series_1: condition.series && condition.series[condition.series.length - 1],
    },
  });
}
console.log(JSON.stringify(results, null, 2));
process.exit(0);
