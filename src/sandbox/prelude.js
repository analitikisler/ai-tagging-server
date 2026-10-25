// Runs once in every template context, before the template. It receives the
// host bindings and returns `require`, the template's `data` and a dispatch
// function the server uses to settle sends and container runs. Nothing it
// receives stays reachable from the global scope.
(function (host, dataJson, isTag, grantsJson, apiPermsJson) {
  'use strict';

  // No code generation from strings.
  delete globalThis.eval;
  const blockCtor = (fn) => {
    Object.defineProperty(Object.getPrototypeOf(fn), 'constructor', {
      value: undefined, writable: false, configurable: false,
    });
  };
  blockCtor(function () {});
  blockCtor(async function () {});
  blockCtor(function* () {});
  blockCtor(async function* () {});

  const notImplemented = (name) => function () {
    throw new Error(name + ' is not implemented yet');
  };

  let request = null;
  const req = () => (request || (request = JSON.parse(host.request())));

  const decode = (s) => {
    try { return decodeURIComponent(String(s).replace(/\+/g, ' ')); } catch (e) { return undefined; }
  };

  const parseQuery = (qs) => {
    const out = {};
    String(qs || '').replace(/^\?/, '').split('&').forEach((pair) => {
      if (!pair) return;
      const i = pair.indexOf('=');
      const k = decode(i < 0 ? pair : pair.slice(0, i));
      const v = decode(i < 0 ? '' : pair.slice(i + 1));
      if (k === undefined) return;
      if (!(k in out)) out[k] = v;
      else if (Array.isArray(out[k])) out[k].push(v);
      else out[k] = [out[k], v];
    });
    return out;
  };

  const headerValues = (name) => {
    const n = String(name).toLowerCase();
    return req().headers.filter((h) => h[0] === n).map((h) => h[1]);
  };

  const getType = (v) => {
    if (v === null) return 'null';
    if (Array.isArray(v)) return 'array';
    return typeof v;
  };

  const makeString = (v) => {
    if (v === undefined) return 'undefined';
    if (v === null) return 'null';
    if (typeof v === 'object') return JSON.stringify(v);
    return String(v);
  };

  // --- Pending sends and container runs, settled by the server -------------
  const pendingSends = {};
  const pendingContainers = {};
  const pendingCallbacks = {};
  const pendingListeners = {};

  const send = (url, options, body, callback) => {
    const opts = options || {};
    const id = host.send(
      String(url),
      JSON.stringify({
        method: opts.method || 'GET',
        headers: opts.headers || {},
        timeout: opts.timeout,
      }),
      body === undefined ? '' : String(body)
    );
    if (callback) {
      pendingSends[id] = (res) => {
        if (res.error) callback(0, {}, '');
        else callback(res.statusCode, res.headers, res.body);
      };
      return undefined;
    }
    return new Promise((resolve, reject) => {
      pendingSends[id] = (res) => {
        if (res.error) reject({ reason: res.error });
        else resolve({ statusCode: res.statusCode, headers: res.headers, body: res.body });
      };
    });
  };

  // --- BigQuery -------------------------------------------------------------
  // BigQuery.insert(connectionInfo, rows, options) streams rows into a table
  // with the server's Google credentials. Resolves with nothing once every
  // row is in; rejects with a list of { reason, message, row? } otherwise.
  // Unlike sGTM, `projectId` is required: the server has no default project.
  const BAD_INSERT = {
    '-1': 'too many sends in one run',
    '-2': 'connectionInfo needs a valid projectId, datasetId and tableId',
    '-3': 'rows must be a list of 1 to 500 objects',
  };
  const bigQueryInsert = (connectionInfo, rows, options) => {
    const c = connectionInfo || {};
    const list = Array.isArray(rows) ? rows : [];
    const o = options || {};
    const id = host.bigqueryInsert(
      String(c.projectId || ''), String(c.datasetId || ''), String(c.tableId || ''),
      JSON.stringify(list),
      JSON.stringify({ ignoreUnknownValues: !!o.ignoreUnknownValues, skipInvalidRows: !!o.skipInvalidRows })
    );
    if (id < 0) return Promise.reject([{ reason: 'invalid', message: BAD_INSERT[String(id)] }]);
    return new Promise((resolve, reject) => {
      pendingSends[id] = (res) => {
        if (res.error) {
          reject([{ reason: res.error }]);
          return;
        }
        let body;
        try { body = JSON.parse(res.body); } catch (e) { body = undefined; }
        if (res.statusCode < 200 || res.statusCode >= 300) {
          const err = (body && body.error) || {};
          const first = (err.errors && err.errors[0]) || {};
          reject([{ reason: first.reason || 'http_' + res.statusCode, message: err.message || '' }]);
          return;
        }
        const failed = (body && body.insertErrors) || [];
        if (!failed.length) {
          resolve();
          return;
        }
        reject(failed.map((f) => {
          const first = (f.errors && f.errors[0]) || {};
          return { row: list[f.index], reason: first.reason || 'invalid', message: first.message || '' };
        }));
      };
    });
  };

  // --- Cookies --------------------------------------------------------------
  const getCookieValues = (name, noDecode) => {
    const out = [];
    headerValues('cookie').forEach((header) => {
      header.split(';').forEach((part) => {
        const i = part.indexOf('=');
        if (i < 0) return;
        if (part.slice(0, i).trim() !== name) return;
        const raw = part.slice(i + 1).trim();
        out.push(noDecode ? raw : (decode(raw) === undefined ? raw : decode(raw)));
      });
    });
    return out;
  };

  const setCookie = (name, value, options, noEncode) => {
    const o = options || {};
    let c = name + '=' + (noEncode ? String(value) : encodeURIComponent(String(value)));
    let domain = o.domain;
    if (domain === 'auto') domain = host.etld1(String(req().headers.filter((h) => h[0] === 'host').map((h) => h[1])[0] || '').split(':')[0]);
    if (domain && domain !== 'none') c += '; Domain=' + domain;
    c += '; Path=' + (o.path || '/');
    if (o['max-age'] !== undefined) c += '; Max-Age=' + o['max-age'];
    if (o.expires) c += '; Expires=' + o.expires;
    if (o.secure) c += '; Secure';
    if (o.httpOnly) c += '; HttpOnly';
    if (o.sameSite) c += '; SameSite=' + o.sameSite;
    if (o.partitioned) c += '; Partitioned';
    host.setHeader('set-cookie', c, true);
  };

  // --- URLs -----------------------------------------------------------------
  const parseUrl = (url) => {
    const m = /^([a-z][a-z0-9+.-]*:)\/\/(?:([^:@/]*)(?::([^@/]*))?@)?([^:/?#]*)(?::(\d+))?([^?#]*)(\?[^#]*)?(#.*)?$/i.exec(String(url));
    if (!m) return undefined;
    const protocol = m[1].toLowerCase();
    const hostname = m[4].toLowerCase();
    const port = m[5] || '';
    return {
      href: String(url),
      origin: protocol + '//' + hostname + (port ? ':' + port : ''),
      protocol,
      username: m[2] || '',
      password: m[3] || '',
      host: hostname + (port ? ':' + port : ''),
      hostname,
      port,
      pathname: m[6] || '/',
      search: m[7] || '',
      searchParams: parseQuery(m[7] || ''),
      hash: m[8] || '',
    };
  };

  const wrapUri = (fn) => (s) => {
    try { return fn(String(s)); } catch (e) { return undefined; }
  };

  // --- Data ----------------------------------------------------------------
  let eventData = null;
  const currentEvent = () => (eventData || (eventData = JSON.parse(host.event())));

  const getEventData = (keyPath) => {
    let v = currentEvent();
    const parts = String(keyPath).split('.');
    for (let i = 0; i < parts.length; i++) {
      if (v === null || typeof v !== 'object') return undefined;
      v = v[parts[i]];
    }
    return v;
  };

  const clone = (v) => (v === undefined ? undefined : JSON.parse(JSON.stringify(v)));

  const apis = {
    // Request
    getRequestMethod: () => req().method,
    getRequestPath: () => req().path,
    getRequestQueryString: () => req().query,
    getRequestQueryParameters: () => parseQuery(req().query),
    getRequestQueryParameter: (name) => {
      const v = parseQuery(req().query)[name];
      return Array.isArray(v) ? v[0] : v;
    },
    getRequestHeader: (name) => {
      const v = headerValues(name);
      return v.length ? v.join(', ') : undefined;
    },
    getRequestBody: () => req().body,
    getRemoteAddress: () => req().remote_address,
    getCookieValues,
    isRequestMpv1: () => host.isMpv1(),
    extractEventsFromMpv1: () => JSON.parse(host.extractMpv1()),
    isRequestMpv2: () => host.isMpv2(),
    extractEventsFromMpv2: () => JSON.parse(host.extractMpv2()),

    // Response
    claimRequest: () => host.claim(),
    setResponseStatus: (code) => host.setStatus(Number(code) | 0),
    setResponseHeader: (name, value) => host.setHeader(String(name).toLowerCase(), String(value), false),
    setResponseBody: (body, encoding) => host.setBody(String(body), encoding === 'base64'),
    setPixelResponse: () => {
      host.setStatus(200);
      host.setHeader('content-type', 'image/gif', false);
      host.setHeader('cache-control', 'no-store', false);
      host.setBody('R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7', true);
    },
    setCookie,
    returnResponse: () => host.returnResponse(),

    // Events and container
    runContainer: (event, onComplete, onStart) => {
      const idx = host.runContainer(JSON.stringify(event || {}));
      if (typeof onStart === 'function') onStart((fn) => fn);
      pendingContainers[idx] = typeof onComplete === 'function' ? onComplete : () => {};
    },
    getAllEventData: () => clone(currentEvent()),
    getEventData,
    getClientName: () => host.clientName(),
    getContainerVersion: () => JSON.parse(host.containerVersion()),

    // Outgoing HTTP
    sendHttpRequest: (url, a, b, c) =>
      (typeof a === 'function' ? send(url, b, c, a) : send(url, a, b)),
    sendHttpGet: (url, a, b) => {
      if (typeof a === 'function') return send(url, Object.assign({}, b, { method: 'GET' }), '', a);
      return send(url, Object.assign({}, a, { method: 'GET' }), '');
    },

    // Strings, URLs and regular expressions
    parseUrl,
    computeEffectiveTldPlusOne: (input) => {
      const s = String(input);
      const h = s.indexOf('://') >= 0 ? (parseUrl(s) || {}).hostname : s;
      return host.etld1(h || '') || undefined;
    },
    encodeUri: wrapUri(encodeURI),
    encodeUriComponent: wrapUri(encodeURIComponent),
    decodeUri: wrapUri(decodeURI),
    decodeUriComponent: wrapUri(decodeURIComponent),
    createRegex: (pattern, flags) => {
      try { return new RegExp(String(pattern), flags ? String(flags) : ''); } catch (e) { return null; }
    },
    testRegex: (re, s) => {
      if (!(re instanceof RegExp)) return false;
      re.lastIndex = 0;
      return re.test(String(s));
    },

    // Types and conversion
    getType,
    makeString,
    makeNumber: (v) => {
      const n = Number(v);
      return isNaN(n) ? undefined : n;
    },
    makeInteger: (v) => {
      const n = Number(v);
      return isNaN(n) ? undefined : (n < 0 ? Math.ceil(n) : Math.floor(n));
    },
    makeTableMap: (table, keyCol, valueCol) => {
      if (!Array.isArray(table)) return null;
      const out = {};
      table.forEach((row) => {
        if (row && row[keyCol] !== undefined) out[row[keyCol]] = row[valueCol];
      });
      return out;
    },

    // Encoding and hashing
    toBase64: (s) => host.toBase64(String(s)),
    fromBase64: (s) => {
      const r = host.fromBase64(String(s));
      return r === '\u0000invalid' ? undefined : r;
    },
    sha256Sync: (input, options) => host.sha256(String(input), (options && options.outputEncoding) || 'base64'),
    sha256: (input, onSuccess, options) => {
      const out = host.sha256(String(input), (options && options.outputEncoding) || 'base64');
      Promise.resolve().then(() => onSuccess(out));
    },

    hmacSha256: (input, key, onSuccess, onFailure, options) => {
      const out = host.hmacSha256(String(input), String(key), (options && options.outputEncoding) || 'base64');
      Promise.resolve().then(() => onSuccess(out));
    },

    // Event callbacks and messages between templates
    addEventCallback: (callback) => {
      if (typeof callback !== 'function') return;
      const id = host.addEventCallback();
      if (id >= 0) pendingCallbacks[id] = callback;
    },
    addMessageListener: (type, callback) => {
      if (typeof callback !== 'function') return;
      const id = host.addListener(String(type));
      if (id >= 0) pendingListeners[id] = callback;
    },
    hasMessageListener: (type) => host.hasListener(String(type)),
    sendMessage: (type, message) => {
      host.sendMessage(String(type), JSON.stringify(message === undefined ? null : message));
    },

    // Time, randomness and logging
    getTimestampMillis: () => Date.now(),
    getTimestamp: () => Date.now(),
    generateRandom: (min, max) => Math.floor(Math.random() * (max - min + 1)) + min,
    callLater: (fn) => { Promise.resolve().then(() => fn()); },
    logToConsole: function () {
      const parts = [];
      for (let i = 0; i < arguments.length; i++) {
        parts.push(typeof arguments[i] === 'string' ? arguments[i] : makeString(arguments[i]));
      }
      host.log(parts.join(' '));
    },

    // Storage shared by every run of this template on this server
    templateDataStorage: Object.freeze({
      getItemCopy: (key) => {
        const v = host.storageGet(String(key));
        return v === '' ? undefined : JSON.parse(v);
      },
      setItemCopy: (key, value) => host.storageSet(String(key), JSON.stringify(value === undefined ? null : value)),
      removeItem: (key) => host.storageRemove(String(key)),
      clear: () => host.storageClear(),
    }),

    BigQuery: Object.freeze({ insert: bigQueryInsert }),

    // Standard objects
    JSON: Object.freeze({
      parse: (s) => { try { return JSON.parse(s); } catch (e) { return undefined; } },
      stringify: (v) => { try { return JSON.stringify(v); } catch (e) { return undefined; } },
    }),
    Math: Object.freeze({
      abs: Math.abs, floor: Math.floor, ceil: Math.ceil, round: Math.round,
      max: Math.max, min: Math.min, pow: Math.pow, sqrt: Math.sqrt,
    }),
    Object: Object.freeze({
      keys: (o) => Object.keys(Object(o)),
      values: (o) => Object.values(Object(o)),
      entries: (o) => Object.entries(Object(o)),
      freeze: (o) => (o !== null && typeof o === 'object' ? Object.freeze(o) : o),
      delete: (o, key) => { if (o !== null && typeof o === 'object') delete o[key]; return o; },
    }),
    Promise: Object.freeze({
      create: (resolver) => new Promise(resolver),
      all: (inputs) => Promise.all(inputs),
    }),

    // Registered, not built yet
    sendEventToGoogleAnalytics: notImplemented('sendEventToGoogleAnalytics'),
    sendPixelFromBrowser: notImplemented('sendPixelFromBrowser'),
    getGoogleScript: notImplemented('getGoogleScript'),
    // The server's Google token never reaches a template; BigQuery.insert
    // adds it on the way out.
    getGoogleAuth: notImplemented('getGoogleAuth'),
    Firestore: Object.freeze({
      read: notImplemented('Firestore.read'),
      write: notImplemented('Firestore.write'),
      query: notImplemented('Firestore.query'),
      runTransaction: notImplemented('Firestore.runTransaction'),
    }),
  };

  Object.keys(apis).forEach((k) => { if (typeof apis[k] === 'function') Object.freeze(apis[k]); });
  Object.freeze(apis);

  // --- Template permissions -------------------------------------------------
  // `grants` maps permission ids to their parameters; null means a trusted
  // built-in template, which may use every API.
  const grants = JSON.parse(grantsJson);
  const apiPerms = JSON.parse(apiPermsJson);

  const globMatch = (pattern, s) =>
    new RegExp('^' + String(pattern).replace(/[.+?^${}()|[\]\\]/g, '\\$&').replace(/\*/g, '.*') + '$').test(s);
  const anyMatch = (list, s) => list.some((p) => globMatch(p, s));

  // Per-call checks for permissions that limit what an API may touch.
  const argChecks = {
    sendHttpRequest: (g, a) => anyMatch(g.urls, String(a[0])),
    sendHttpGet: (g, a) => anyMatch(g.urls, String(a[0])),
    getEventData: (g, a) => anyMatch(g.keys, String(a[0])),
    getAllEventData: (g) => g.keys.indexOf('*') >= 0,
    getCookieValues: (g, a) => anyMatch(g.names, String(a[0])),
    setCookie: (g, a) => anyMatch(g.names, String(a[0])),
  };

  const require = (name) => {
    const key = String(name);
    if (key.slice(0, 2) === '__' || !Object.prototype.hasOwnProperty.call(apis, key)) {
      throw new Error('require("' + key + '") is not a permitted API');
    }
    const perm = apiPerms[key];
    if (!grants || !perm) return apis[key];
    const g = grants[perm];
    if (!g) throw new Error(key + ' needs the "' + perm + '" permission');
    if (key === 'BigQuery') {
      return Object.freeze({
        insert: Object.freeze(function (connectionInfo) {
          const c = connectionInfo || {};
          const table = [c.projectId, c.datasetId, c.tableId].map((v) => String(v || '')).join('.');
          if (!anyMatch(g.tables, table)) {
            throw new Error('BigQuery.insert is not allowed for ' + table + ' by the "' + perm + '" permission');
          }
          return apis.BigQuery.insert.apply(undefined, arguments);
        }),
      });
    }
    const check = argChecks[key];
    if (!check) return apis[key];
    return Object.freeze(function () {
      if (!check(g, arguments)) {
        throw new Error(key + ' is not allowed for ' + (arguments.length ? String(arguments[0]) : 'all data') + ' by the "' + perm + '" permission');
      }
      return apis[key].apply(undefined, arguments);
    });
  };

  const data = JSON.parse(dataJson);
  if (isTag) {
    data.gtmOnSuccess = () => host.tagDone(true);
    data.gtmOnFailure = () => host.tagDone(false);
    // Not in sGTM: ends the tag like gtmOnSuccess, but the log line and
    // counters show it as skipped, with the reason.
    data.gtmOnSkipped = (reason) => host.tagSkipped(String(reason));
  }

  const dispatch = (type, id, json) => {
    if (type === 'send') {
      const settle = pendingSends[id];
      delete pendingSends[id];
      if (settle) settle(JSON.parse(json));
    } else if (type === 'eventCallback') {
      const cb = pendingCallbacks[id];
      delete pendingCallbacks[id];
      if (cb) cb('', JSON.parse(json));
    } else if (type === 'message') {
      const cb = pendingListeners[id];
      const m = JSON.parse(json);
      if (cb) cb(m.type, JSON.parse(m.message));
    } else if (type === 'container') {
      const done = pendingContainers[id];
      delete pendingContainers[id];
      if (done) done();
    }
  };

  return { require, data, dispatch };
})
