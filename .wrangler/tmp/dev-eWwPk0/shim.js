var __defProp = Object.defineProperty;
var __name = (target, value) => __defProp(target, "name", { value, configurable: true });

// build/index.js
import { WorkerEntrypoint as gt } from "cloudflare:workers";
import N from "./5936c0808f80f6fcfe6f6469f1fc11e1c8c729ab-index_bg.wasm";
var m = class {
  static {
    __name(this, "m");
  }
  __destroy_into_raw() {
    let t = this.__wbg_ptr;
    return this.__wbg_ptr = 0, et.unregister(this), t;
  }
  free() {
    let t = this.__destroy_into_raw();
    i.__wbg_containerstartupoptions_free(t, 0);
  }
  get enableInternet() {
    if (this.__wbg_inst !== void 0 && this.__wbg_inst !== o) throw new Error("Invalid stale object from previous Wasm instance");
    let t = i.__wbg_get_containerstartupoptions_enableInternet(this.__wbg_ptr);
    return t === 16777215 ? void 0 : t !== 0;
  }
  get entrypoint() {
    if (this.__wbg_inst !== void 0 && this.__wbg_inst !== o) throw new Error("Invalid stale object from previous Wasm instance");
    let t = i.__wbg_get_containerstartupoptions_entrypoint(this.__wbg_ptr);
    var e = st(t[0], t[1]).slice();
    return i.__wbindgen_free(t[0], t[1] * 4, 4), e;
  }
  get env() {
    if (this.__wbg_inst !== void 0 && this.__wbg_inst !== o) throw new Error("Invalid stale object from previous Wasm instance");
    return i.__wbg_get_containerstartupoptions_env(this.__wbg_ptr);
  }
  set enableInternet(t) {
    if (this.__wbg_inst !== void 0 && this.__wbg_inst !== o) throw new Error("Invalid stale object from previous Wasm instance");
    i.__wbg_set_containerstartupoptions_enableInternet(this.__wbg_ptr, a(t) ? 16777215 : t ? 1 : 0);
  }
  set entrypoint(t) {
    if (this.__wbg_inst !== void 0 && this.__wbg_inst !== o) throw new Error("Invalid stale object from previous Wasm instance");
    let e = ct(t, i.__wbindgen_malloc), r = d;
    i.__wbg_set_containerstartupoptions_entrypoint(this.__wbg_ptr, e, r);
  }
  set env(t) {
    if (this.__wbg_inst !== void 0 && this.__wbg_inst !== o) throw new Error("Invalid stale object from previous Wasm instance");
    i.__wbg_set_containerstartupoptions_env(this.__wbg_ptr, t);
  }
};
Symbol.dispose && (m.prototype[Symbol.dispose] = m.prototype.free);
var x = class {
  static {
    __name(this, "x");
  }
  __destroy_into_raw() {
    let t = this.__wbg_ptr;
    return this.__wbg_ptr = 0, nt.unregister(this), t;
  }
  free() {
    let t = this.__destroy_into_raw();
    i.__wbg_intounderlyingbytesource_free(t, 0);
  }
  get autoAllocateChunkSize() {
    if (this.__wbg_inst !== void 0 && this.__wbg_inst !== o) throw new Error("Invalid stale object from previous Wasm instance");
    return i.intounderlyingbytesource_autoAllocateChunkSize(this.__wbg_ptr) >>> 0;
  }
  cancel() {
    if (this.__wbg_inst !== void 0 && this.__wbg_inst !== o) throw new Error("Invalid stale object from previous Wasm instance");
    let t = this.__destroy_into_raw();
    i.intounderlyingbytesource_cancel(t);
  }
  pull(t) {
    if (this.__wbg_inst !== void 0 && this.__wbg_inst !== o) throw new Error("Invalid stale object from previous Wasm instance");
    return i.intounderlyingbytesource_pull(this.__wbg_ptr, t);
  }
  start(t) {
    if (this.__wbg_inst !== void 0 && this.__wbg_inst !== o) throw new Error("Invalid stale object from previous Wasm instance");
    i.intounderlyingbytesource_start(this.__wbg_ptr, t);
  }
  get type() {
    if (this.__wbg_inst !== void 0 && this.__wbg_inst !== o) throw new Error("Invalid stale object from previous Wasm instance");
    let t = i.intounderlyingbytesource_type(this.__wbg_ptr);
    return X[t];
  }
};
Symbol.dispose && (x.prototype[Symbol.dispose] = x.prototype.free);
var v = class {
  static {
    __name(this, "v");
  }
  __destroy_into_raw() {
    let t = this.__wbg_ptr;
    return this.__wbg_ptr = 0, rt.unregister(this), t;
  }
  free() {
    let t = this.__destroy_into_raw();
    i.__wbg_intounderlyingsink_free(t, 0);
  }
  abort(t) {
    if (this.__wbg_inst !== void 0 && this.__wbg_inst !== o) throw new Error("Invalid stale object from previous Wasm instance");
    let e = this.__destroy_into_raw();
    return i.intounderlyingsink_abort(e, t);
  }
  close() {
    if (this.__wbg_inst !== void 0 && this.__wbg_inst !== o) throw new Error("Invalid stale object from previous Wasm instance");
    let t = this.__destroy_into_raw();
    return i.intounderlyingsink_close(t);
  }
  write(t) {
    if (this.__wbg_inst !== void 0 && this.__wbg_inst !== o) throw new Error("Invalid stale object from previous Wasm instance");
    return i.intounderlyingsink_write(this.__wbg_ptr, t);
  }
};
Symbol.dispose && (v.prototype[Symbol.dispose] = v.prototype.free);
var I = class {
  static {
    __name(this, "I");
  }
  __destroy_into_raw() {
    let t = this.__wbg_ptr;
    return this.__wbg_ptr = 0, _t.unregister(this), t;
  }
  free() {
    let t = this.__destroy_into_raw();
    i.__wbg_intounderlyingsource_free(t, 0);
  }
  cancel() {
    if (this.__wbg_inst !== void 0 && this.__wbg_inst !== o) throw new Error("Invalid stale object from previous Wasm instance");
    let t = this.__destroy_into_raw();
    i.intounderlyingsource_cancel(t);
  }
  pull(t) {
    if (this.__wbg_inst !== void 0 && this.__wbg_inst !== o) throw new Error("Invalid stale object from previous Wasm instance");
    return i.intounderlyingsource_pull(this.__wbg_ptr, t);
  }
};
Symbol.dispose && (I.prototype[Symbol.dispose] = I.prototype.free);
var R = class {
  static {
    __name(this, "R");
  }
  __destroy_into_raw() {
    let t = this.__wbg_ptr;
    return this.__wbg_ptr = 0, it.unregister(this), t;
  }
  free() {
    let t = this.__destroy_into_raw();
    i.__wbg_minifyconfig_free(t, 0);
  }
  get css() {
    if (this.__wbg_inst !== void 0 && this.__wbg_inst !== o) throw new Error("Invalid stale object from previous Wasm instance");
    return i.__wbg_get_minifyconfig_css(this.__wbg_ptr) !== 0;
  }
  get html() {
    if (this.__wbg_inst !== void 0 && this.__wbg_inst !== o) throw new Error("Invalid stale object from previous Wasm instance");
    return i.__wbg_get_minifyconfig_html(this.__wbg_ptr) !== 0;
  }
  get js() {
    if (this.__wbg_inst !== void 0 && this.__wbg_inst !== o) throw new Error("Invalid stale object from previous Wasm instance");
    return i.__wbg_get_minifyconfig_js(this.__wbg_ptr) !== 0;
  }
  set css(t) {
    if (this.__wbg_inst !== void 0 && this.__wbg_inst !== o) throw new Error("Invalid stale object from previous Wasm instance");
    i.__wbg_set_minifyconfig_css(this.__wbg_ptr, t);
  }
  set html(t) {
    if (this.__wbg_inst !== void 0 && this.__wbg_inst !== o) throw new Error("Invalid stale object from previous Wasm instance");
    i.__wbg_set_minifyconfig_html(this.__wbg_ptr, t);
  }
  set js(t) {
    if (this.__wbg_inst !== void 0 && this.__wbg_inst !== o) throw new Error("Invalid stale object from previous Wasm instance");
    i.__wbg_set_minifyconfig_js(this.__wbg_ptr, t);
  }
};
Symbol.dispose && (R.prototype[Symbol.dispose] = R.prototype.free);
var E = class {
  static {
    __name(this, "E");
  }
  __destroy_into_raw() {
    let t = this.__wbg_ptr;
    return this.__wbg_ptr = 0, ot.unregister(this), t;
  }
  free() {
    let t = this.__destroy_into_raw();
    i.__wbg_r2range_free(t, 0);
  }
  get length() {
    if (this.__wbg_inst !== void 0 && this.__wbg_inst !== o) throw new Error("Invalid stale object from previous Wasm instance");
    let t = i.__wbg_get_r2range_length(this.__wbg_ptr);
    return t[0] === 0 ? void 0 : t[1];
  }
  get offset() {
    if (this.__wbg_inst !== void 0 && this.__wbg_inst !== o) throw new Error("Invalid stale object from previous Wasm instance");
    let t = i.__wbg_get_r2range_offset(this.__wbg_ptr);
    return t[0] === 0 ? void 0 : t[1];
  }
  get suffix() {
    if (this.__wbg_inst !== void 0 && this.__wbg_inst !== o) throw new Error("Invalid stale object from previous Wasm instance");
    let t = i.__wbg_get_r2range_suffix(this.__wbg_ptr);
    return t[0] === 0 ? void 0 : t[1];
  }
  set length(t) {
    if (this.__wbg_inst !== void 0 && this.__wbg_inst !== o) throw new Error("Invalid stale object from previous Wasm instance");
    i.__wbg_set_r2range_length(this.__wbg_ptr, !a(t), a(t) ? 0 : t);
  }
  set offset(t) {
    if (this.__wbg_inst !== void 0 && this.__wbg_inst !== o) throw new Error("Invalid stale object from previous Wasm instance");
    i.__wbg_set_r2range_offset(this.__wbg_ptr, !a(t), a(t) ? 0 : t);
  }
  set suffix(t) {
    if (this.__wbg_inst !== void 0 && this.__wbg_inst !== o) throw new Error("Invalid stale object from previous Wasm instance");
    i.__wbg_set_r2range_suffix(this.__wbg_ptr, !a(t), a(t) ? 0 : t);
  }
};
Symbol.dispose && (E.prototype[Symbol.dispose] = E.prototype.free);
function H() {
  o++, p = null, W = null, typeof numBytesDecoded < "u" && (numBytesDecoded = 0), typeof d < "u" && (d = 0), i = new WebAssembly.Instance(N, B()).exports, i.__wbindgen_start();
}
__name(H, "H");
function V(n, t, e) {
  return i.fetch(n, t, e);
}
__name(V, "V");
function C(n) {
  i.setPanicHook(n);
}
__name(C, "C");
function B() {
  return { __proto__: null, "./index_bg.js": { __proto__: null, __wbg_String_8564e559799eccda: /* @__PURE__ */ __name(function(t, e) {
    let r = String(e), _ = y(r, i.__wbindgen_malloc, i.__wbindgen_realloc), s = d;
    b().setInt32(t + 4, s, true), b().setInt32(t + 0, _, true);
  }, "__wbg_String_8564e559799eccda"), __wbg___wbindgen_debug_string_5398f5bb970e0daa: /* @__PURE__ */ __name(function(t, e) {
    let r = M(e), _ = y(r, i.__wbindgen_malloc, i.__wbindgen_realloc), s = d;
    b().setInt32(t + 4, s, true), b().setInt32(t + 0, _, true);
  }, "__wbg___wbindgen_debug_string_5398f5bb970e0daa"), __wbg___wbindgen_is_function_3c846841762788c1: /* @__PURE__ */ __name(function(t) {
    return typeof t == "function";
  }, "__wbg___wbindgen_is_function_3c846841762788c1"), __wbg___wbindgen_is_object_781bc9f159099513: /* @__PURE__ */ __name(function(t) {
    let e = t;
    return typeof e == "object" && e !== null;
  }, "__wbg___wbindgen_is_object_781bc9f159099513"), __wbg___wbindgen_is_undefined_52709e72fb9f179c: /* @__PURE__ */ __name(function(t) {
    return t === void 0;
  }, "__wbg___wbindgen_is_undefined_52709e72fb9f179c"), __wbg___wbindgen_string_get_395e606bd0ee4427: /* @__PURE__ */ __name(function(t, e) {
    let r = e, _ = typeof r == "string" ? r : void 0;
    var s = a(_) ? 0 : y(_, i.__wbindgen_malloc, i.__wbindgen_realloc), c = d;
    b().setInt32(t + 4, c, true), b().setInt32(t + 0, s, true);
  }, "__wbg___wbindgen_string_get_395e606bd0ee4427"), __wbg___wbindgen_throw_6ddd609b62940d55: /* @__PURE__ */ __name(function(t, e) {
    throw new Error(w(t, e));
  }, "__wbg___wbindgen_throw_6ddd609b62940d55"), __wbg__wbg_cb_unref_6b5b6b8576d35cb1: /* @__PURE__ */ __name(function(t) {
    t._wbg_cb_unref();
  }, "__wbg__wbg_cb_unref_6b5b6b8576d35cb1"), __wbg_abort_5ef96933660780b7: /* @__PURE__ */ __name(function(t) {
    t.abort();
  }, "__wbg_abort_5ef96933660780b7"), __wbg_abort_6479c2d794ebf2ee: /* @__PURE__ */ __name(function(t, e) {
    t.abort(e);
  }, "__wbg_abort_6479c2d794ebf2ee"), __wbg_append_608dfb635ee8998f: /* @__PURE__ */ __name(function() {
    return u(function(t, e, r, _, s) {
      t.append(w(e, r), w(_, s));
    }, arguments);
  }, "__wbg_append_608dfb635ee8998f"), __wbg_buffer_60b8043cd926067d: /* @__PURE__ */ __name(function(t) {
    return t.buffer;
  }, "__wbg_buffer_60b8043cd926067d"), __wbg_byobRequest_6342e5f2b232c0f9: /* @__PURE__ */ __name(function(t) {
    let e = t.byobRequest;
    return a(e) ? 0 : l(e);
  }, "__wbg_byobRequest_6342e5f2b232c0f9"), __wbg_byteLength_607b856aa6c5a508: /* @__PURE__ */ __name(function(t) {
    return t.byteLength;
  }, "__wbg_byteLength_607b856aa6c5a508"), __wbg_byteOffset_b26b63681c83856c: /* @__PURE__ */ __name(function(t) {
    return t.byteOffset;
  }, "__wbg_byteOffset_b26b63681c83856c"), __wbg_call_2d781c1f4d5c0ef8: /* @__PURE__ */ __name(function() {
    return u(function(t, e, r) {
      return t.call(e, r);
    }, arguments);
  }, "__wbg_call_2d781c1f4d5c0ef8"), __wbg_call_e133b57c9155d22c: /* @__PURE__ */ __name(function() {
    return u(function(t, e) {
      return t.call(e);
    }, arguments);
  }, "__wbg_call_e133b57c9155d22c"), __wbg_cause_f02a23068e3256fa: /* @__PURE__ */ __name(function(t) {
    return t.cause;
  }, "__wbg_cause_f02a23068e3256fa"), __wbg_cf_c5a23ee8e524d1e1: /* @__PURE__ */ __name(function() {
    return u(function(t) {
      let e = t.cf;
      return a(e) ? 0 : l(e);
    }, arguments);
  }, "__wbg_cf_c5a23ee8e524d1e1"), __wbg_clearTimeout_6b8d9a38b9263d65: /* @__PURE__ */ __name(function(t) {
    return clearTimeout(t);
  }, "__wbg_clearTimeout_6b8d9a38b9263d65"), __wbg_close_690d36108c557337: /* @__PURE__ */ __name(function() {
    return u(function(t) {
      t.close();
    }, arguments);
  }, "__wbg_close_690d36108c557337"), __wbg_close_737b4b1fbc658540: /* @__PURE__ */ __name(function() {
    return u(function(t) {
      t.close();
    }, arguments);
  }, "__wbg_close_737b4b1fbc658540"), __wbg_constructor_b66dd7209f26ae23: /* @__PURE__ */ __name(function(t) {
    return t.constructor;
  }, "__wbg_constructor_b66dd7209f26ae23"), __wbg_done_08ce71ee07e3bd17: /* @__PURE__ */ __name(function(t) {
    return t.done;
  }, "__wbg_done_08ce71ee07e3bd17"), __wbg_enqueue_ec3552838b4b7fbf: /* @__PURE__ */ __name(function() {
    return u(function(t, e) {
      t.enqueue(e);
    }, arguments);
  }, "__wbg_enqueue_ec3552838b4b7fbf"), __wbg_error_8d9a8e04cd1d3588: /* @__PURE__ */ __name(function(t) {
    console.error(t);
  }, "__wbg_error_8d9a8e04cd1d3588"), __wbg_error_cfce0f619500de52: /* @__PURE__ */ __name(function(t, e) {
    console.error(t, e);
  }, "__wbg_error_cfce0f619500de52"), __wbg_fetch_5550a88cf343aaa9: /* @__PURE__ */ __name(function(t, e) {
    return t.fetch(e);
  }, "__wbg_fetch_5550a88cf343aaa9"), __wbg_fetch_9dad4fe911207b37: /* @__PURE__ */ __name(function(t) {
    return fetch(t);
  }, "__wbg_fetch_9dad4fe911207b37"), __wbg_getRandomValues_76dfc69825c9c552: /* @__PURE__ */ __name(function() {
    return u(function(t, e) {
      globalThis.crypto.getRandomValues(A(t, e));
    }, arguments);
  }, "__wbg_getRandomValues_76dfc69825c9c552"), __wbg_getTime_1dad7b5386ddd2d9: /* @__PURE__ */ __name(function(t) {
    return t.getTime();
  }, "__wbg_getTime_1dad7b5386ddd2d9"), __wbg_get_326e41e095fb2575: /* @__PURE__ */ __name(function() {
    return u(function(t, e) {
      return Reflect.get(t, e);
    }, arguments);
  }, "__wbg_get_326e41e095fb2575"), __wbg_get_3ef1eba1850ade27: /* @__PURE__ */ __name(function() {
    return u(function(t, e) {
      return Reflect.get(t, e);
    }, arguments);
  }, "__wbg_get_3ef1eba1850ade27"), __wbg_has_926ef2ff40b308cf: /* @__PURE__ */ __name(function() {
    return u(function(t, e) {
      return Reflect.has(t, e);
    }, arguments);
  }, "__wbg_has_926ef2ff40b308cf"), __wbg_headers_eb2234545f9ff993: /* @__PURE__ */ __name(function(t) {
    return t.headers;
  }, "__wbg_headers_eb2234545f9ff993"), __wbg_headers_fc8c672cd757e0fd: /* @__PURE__ */ __name(function(t) {
    return t.headers;
  }, "__wbg_headers_fc8c672cd757e0fd"), __wbg_instanceof_Error_4691a5b466e32a80: /* @__PURE__ */ __name(function(t) {
    let e;
    try {
      e = t instanceof Error;
    } catch {
      e = false;
    }
    return e;
  }, "__wbg_instanceof_Error_4691a5b466e32a80"), __wbg_instanceof_Response_9b4d9fd451e051b1: /* @__PURE__ */ __name(function(t) {
    let e;
    try {
      e = t instanceof Response;
    } catch {
      e = false;
    }
    return e;
  }, "__wbg_instanceof_Response_9b4d9fd451e051b1"), __wbg_iterator_d8f549ec8fb061b1: /* @__PURE__ */ __name(function() {
    return Symbol.iterator;
  }, "__wbg_iterator_d8f549ec8fb061b1"), __wbg_length_ea16607d7b61445b: /* @__PURE__ */ __name(function(t) {
    return t.length;
  }, "__wbg_length_ea16607d7b61445b"), __wbg_method_23aa7d0d6ec9a08f: /* @__PURE__ */ __name(function(t, e) {
    let r = e.method, _ = y(r, i.__wbindgen_malloc, i.__wbindgen_realloc), s = d;
    b().setInt32(t + 4, s, true), b().setInt32(t + 0, _, true);
  }, "__wbg_method_23aa7d0d6ec9a08f"), __wbg_name_0bfa6ee19bce1bf9: /* @__PURE__ */ __name(function(t) {
    return t.name;
  }, "__wbg_name_0bfa6ee19bce1bf9"), __wbg_new_0837727332ac86ba: /* @__PURE__ */ __name(function() {
    return u(function() {
      return new Headers();
    }, arguments);
  }, "__wbg_new_0837727332ac86ba"), __wbg_new_0_1dcafdf5e786e876: /* @__PURE__ */ __name(function() {
    return /* @__PURE__ */ new Date();
  }, "__wbg_new_0_1dcafdf5e786e876"), __wbg_new_ab79df5bd7c26067: /* @__PURE__ */ __name(function() {
    return new Object();
  }, "__wbg_new_ab79df5bd7c26067"), __wbg_new_c518c60af666645b: /* @__PURE__ */ __name(function() {
    return u(function() {
      return new AbortController();
    }, arguments);
  }, "__wbg_new_c518c60af666645b"), __wbg_new_d15cb560a6a0e5f0: /* @__PURE__ */ __name(function(t, e) {
    return new Error(w(t, e));
  }, "__wbg_new_d15cb560a6a0e5f0"), __wbg_new_from_slice_22da9388ac046e50: /* @__PURE__ */ __name(function(t, e) {
    return new Uint8Array(A(t, e));
  }, "__wbg_new_from_slice_22da9388ac046e50"), __wbg_new_typed_aaaeaf29cf802876: /* @__PURE__ */ __name(function(t, e) {
    try {
      var r = { a: t, b: e }, _ = /* @__PURE__ */ __name((c, f) => {
        let g = r.a;
        r.a = 0;
        try {
          return Q(g, r.b, c, f);
        } finally {
          r.a = g;
        }
      }, "_");
      return new Promise(_);
    } finally {
      r.a = r.b = 0;
    }
  }, "__wbg_new_typed_aaaeaf29cf802876"), __wbg_new_with_byte_offset_and_length_b2ec5bf7b2f35743: /* @__PURE__ */ __name(function(t, e, r) {
    return new Uint8Array(t, e >>> 0, r >>> 0);
  }, "__wbg_new_with_byte_offset_and_length_b2ec5bf7b2f35743"), __wbg_new_with_length_825018a1616e9e55: /* @__PURE__ */ __name(function(t) {
    return new Uint8Array(t >>> 0);
  }, "__wbg_new_with_length_825018a1616e9e55"), __wbg_new_with_opt_buffer_source_and_init_cbf3b8468cedbba9: /* @__PURE__ */ __name(function() {
    return u(function(t, e) {
      return new Response(t, e);
    }, arguments);
  }, "__wbg_new_with_opt_buffer_source_and_init_cbf3b8468cedbba9"), __wbg_new_with_opt_readable_stream_and_init_15b79ab5fa39d080: /* @__PURE__ */ __name(function() {
    return u(function(t, e) {
      return new Response(t, e);
    }, arguments);
  }, "__wbg_new_with_opt_readable_stream_and_init_15b79ab5fa39d080"), __wbg_new_with_opt_str_and_init_a1ea8e111a765950: /* @__PURE__ */ __name(function() {
    return u(function(t, e, r) {
      return new Response(t === 0 ? void 0 : w(t, e), r);
    }, arguments);
  }, "__wbg_new_with_opt_str_and_init_a1ea8e111a765950"), __wbg_new_with_str_and_init_b4b54d1a819bc724: /* @__PURE__ */ __name(function() {
    return u(function(t, e, r) {
      return new Request(w(t, e), r);
    }, arguments);
  }, "__wbg_new_with_str_and_init_b4b54d1a819bc724"), __wbg_next_11b99ee6237339e3: /* @__PURE__ */ __name(function() {
    return u(function(t) {
      return t.next();
    }, arguments);
  }, "__wbg_next_11b99ee6237339e3"), __wbg_next_e01a967809d1aa68: /* @__PURE__ */ __name(function(t) {
    return t.next;
  }, "__wbg_next_e01a967809d1aa68"), __wbg_queueMicrotask_0c399741342fb10f: /* @__PURE__ */ __name(function(t) {
    return t.queueMicrotask;
  }, "__wbg_queueMicrotask_0c399741342fb10f"), __wbg_queueMicrotask_a082d78ce798393e: /* @__PURE__ */ __name(function(t) {
    queueMicrotask(t);
  }, "__wbg_queueMicrotask_a082d78ce798393e"), __wbg_resolve_ae8d83246e5bcc12: /* @__PURE__ */ __name(function(t) {
    return Promise.resolve(t);
  }, "__wbg_resolve_ae8d83246e5bcc12"), __wbg_respond_e286ee502e7cf7e4: /* @__PURE__ */ __name(function() {
    return u(function(t, e) {
      t.respond(e >>> 0);
    }, arguments);
  }, "__wbg_respond_e286ee502e7cf7e4"), __wbg_setTimeout_f757f00851f76c42: /* @__PURE__ */ __name(function(t, e) {
    return setTimeout(t, e);
  }, "__wbg_setTimeout_f757f00851f76c42"), __wbg_set_7eaa4f96924fd6b3: /* @__PURE__ */ __name(function() {
    return u(function(t, e, r) {
      return Reflect.set(t, e, r);
    }, arguments);
  }, "__wbg_set_7eaa4f96924fd6b3"), __wbg_set_8c0b3ffcf05d61c2: /* @__PURE__ */ __name(function(t, e, r) {
    t.set(A(e, r));
  }, "__wbg_set_8c0b3ffcf05d61c2"), __wbg_set_body_a3d856b097dfda04: /* @__PURE__ */ __name(function(t, e) {
    t.body = e;
  }, "__wbg_set_body_a3d856b097dfda04"), __wbg_set_cache_ec7e430c6056ebda: /* @__PURE__ */ __name(function(t, e) {
    t.cache = Y[e];
  }, "__wbg_set_cache_ec7e430c6056ebda"), __wbg_set_credentials_ed63183445882c65: /* @__PURE__ */ __name(function(t, e) {
    t.credentials = Z[e];
  }, "__wbg_set_credentials_ed63183445882c65"), __wbg_set_e09648bea3f1af1e: /* @__PURE__ */ __name(function() {
    return u(function(t, e, r, _, s) {
      t.set(w(e, r), w(_, s));
    }, arguments);
  }, "__wbg_set_e09648bea3f1af1e"), __wbg_set_headers_3c8fecc693b75327: /* @__PURE__ */ __name(function(t, e) {
    t.headers = e;
  }, "__wbg_set_headers_3c8fecc693b75327"), __wbg_set_headers_bf56980ea1a65acb: /* @__PURE__ */ __name(function(t, e) {
    t.headers = e;
  }, "__wbg_set_headers_bf56980ea1a65acb"), __wbg_set_method_8c015e8bcafd7be1: /* @__PURE__ */ __name(function(t, e, r) {
    t.method = w(e, r);
  }, "__wbg_set_method_8c015e8bcafd7be1"), __wbg_set_mode_5a87f2c809cf37c2: /* @__PURE__ */ __name(function(t, e) {
    t.mode = tt[e];
  }, "__wbg_set_mode_5a87f2c809cf37c2"), __wbg_set_signal_0cebecb698f25d21: /* @__PURE__ */ __name(function(t, e) {
    t.signal = e;
  }, "__wbg_set_signal_0cebecb698f25d21"), __wbg_set_status_b80d37d9d23276c4: /* @__PURE__ */ __name(function(t, e) {
    t.status = e;
  }, "__wbg_set_status_b80d37d9d23276c4"), __wbg_signal_166e1da31adcac18: /* @__PURE__ */ __name(function(t) {
    return t.signal;
  }, "__wbg_signal_166e1da31adcac18"), __wbg_static_accessor_GLOBAL_8adb955bd33fac2f: /* @__PURE__ */ __name(function() {
    let t = typeof global > "u" ? null : global;
    return a(t) ? 0 : l(t);
  }, "__wbg_static_accessor_GLOBAL_8adb955bd33fac2f"), __wbg_static_accessor_GLOBAL_THIS_ad356e0db91c7913: /* @__PURE__ */ __name(function() {
    let t = typeof globalThis > "u" ? null : globalThis;
    return a(t) ? 0 : l(t);
  }, "__wbg_static_accessor_GLOBAL_THIS_ad356e0db91c7913"), __wbg_static_accessor_SELF_f207c857566db248: /* @__PURE__ */ __name(function() {
    let t = typeof self > "u" ? null : self;
    return a(t) ? 0 : l(t);
  }, "__wbg_static_accessor_SELF_f207c857566db248"), __wbg_static_accessor_WINDOW_bb9f1ba69d61b386: /* @__PURE__ */ __name(function() {
    let t = typeof window > "u" ? null : window;
    return a(t) ? 0 : l(t);
  }, "__wbg_static_accessor_WINDOW_bb9f1ba69d61b386"), __wbg_status_318629ab93a22955: /* @__PURE__ */ __name(function(t) {
    return t.status;
  }, "__wbg_status_318629ab93a22955"), __wbg_stringify_5ae93966a84901ac: /* @__PURE__ */ __name(function() {
    return u(function(t) {
      return JSON.stringify(t);
    }, arguments);
  }, "__wbg_stringify_5ae93966a84901ac"), __wbg_text_372f5b91442c50f9: /* @__PURE__ */ __name(function() {
    return u(function(t) {
      return t.text();
    }, arguments);
  }, "__wbg_text_372f5b91442c50f9"), __wbg_text_5415c19219ba815b: /* @__PURE__ */ __name(function() {
    return u(function(t) {
      return t.text();
    }, arguments);
  }, "__wbg_text_5415c19219ba815b"), __wbg_then_098abe61755d12f6: /* @__PURE__ */ __name(function(t, e) {
    return t.then(e);
  }, "__wbg_then_098abe61755d12f6"), __wbg_then_9e335f6dd892bc11: /* @__PURE__ */ __name(function(t, e, r) {
    return t.then(e, r);
  }, "__wbg_then_9e335f6dd892bc11"), __wbg_toString_fca8b5e46235cfb4: /* @__PURE__ */ __name(function(t) {
    return t.toString();
  }, "__wbg_toString_fca8b5e46235cfb4"), __wbg_url_7fefc1820fba4e0c: /* @__PURE__ */ __name(function(t, e) {
    let r = e.url, _ = y(r, i.__wbindgen_malloc, i.__wbindgen_realloc), s = d;
    b().setInt32(t + 4, s, true), b().setInt32(t + 0, _, true);
  }, "__wbg_url_7fefc1820fba4e0c"), __wbg_url_b6f96880b733816c: /* @__PURE__ */ __name(function(t, e) {
    let r = e.url, _ = y(r, i.__wbindgen_malloc, i.__wbindgen_realloc), s = d;
    b().setInt32(t + 4, s, true), b().setInt32(t + 0, _, true);
  }, "__wbg_url_b6f96880b733816c"), __wbg_value_21fc78aab0322612: /* @__PURE__ */ __name(function(t) {
    return t.value;
  }, "__wbg_value_21fc78aab0322612"), __wbg_view_f68a712e7315f8b2: /* @__PURE__ */ __name(function(t) {
    let e = t.view;
    return a(e) ? 0 : l(e);
  }, "__wbg_view_f68a712e7315f8b2"), __wbindgen_cast_0000000000000001: /* @__PURE__ */ __name(function(t, e) {
    return D(t, e, i.wasm_bindgen__closure__destroy__h1549a925fbbd2a72, G);
  }, "__wbindgen_cast_0000000000000001"), __wbindgen_cast_0000000000000002: /* @__PURE__ */ __name(function(t, e) {
    return D(t, e, i.wasm_bindgen__closure__destroy__hca121f245e789376, K);
  }, "__wbindgen_cast_0000000000000002"), __wbindgen_cast_0000000000000003: /* @__PURE__ */ __name(function(t, e) {
    return w(t, e);
  }, "__wbindgen_cast_0000000000000003"), __wbindgen_init_externref_table: /* @__PURE__ */ __name(function() {
    let t = i.__wbindgen_externrefs, e = t.grow(4);
    t.set(0, void 0), t.set(e + 0, void 0), t.set(e + 1, null), t.set(e + 2, true), t.set(e + 3, false);
  }, "__wbindgen_init_externref_table") } };
}
__name(B, "B");
function G(n, t) {
  i.wasm_bindgen__convert__closures_____invoke__h30f00a8bdad268b3(n, t);
}
__name(G, "G");
function K(n, t, e) {
  let r = i.wasm_bindgen__convert__closures_____invoke__h11e07726110fedeb(n, t, e);
  if (r[1]) throw ut(r[0]);
}
__name(K, "K");
function Q(n, t, e, r) {
  i.wasm_bindgen__convert__closures_____invoke__h1f53f5b6ddb4bd42(n, t, e, r);
}
__name(Q, "Q");
var X = ["bytes"];
var Y = ["default", "no-store", "reload", "no-cache", "force-cache", "only-if-cached"];
var Z = ["omit", "same-origin", "include"];
var tt = ["same-origin", "no-cors", "cors", "navigate"];
var o = 0;
var et = typeof FinalizationRegistry > "u" ? { register: /* @__PURE__ */ __name(() => {
}, "register"), unregister: /* @__PURE__ */ __name(() => {
}, "unregister") } : new FinalizationRegistry(({ ptr: n, instance: t }) => {
  t === o && i.__wbg_containerstartupoptions_free(n >>> 0, 1);
});
var nt = typeof FinalizationRegistry > "u" ? { register: /* @__PURE__ */ __name(() => {
}, "register"), unregister: /* @__PURE__ */ __name(() => {
}, "unregister") } : new FinalizationRegistry(({ ptr: n, instance: t }) => {
  t === o && i.__wbg_intounderlyingbytesource_free(n >>> 0, 1);
});
var rt = typeof FinalizationRegistry > "u" ? { register: /* @__PURE__ */ __name(() => {
}, "register"), unregister: /* @__PURE__ */ __name(() => {
}, "unregister") } : new FinalizationRegistry(({ ptr: n, instance: t }) => {
  t === o && i.__wbg_intounderlyingsink_free(n >>> 0, 1);
});
var _t = typeof FinalizationRegistry > "u" ? { register: /* @__PURE__ */ __name(() => {
}, "register"), unregister: /* @__PURE__ */ __name(() => {
}, "unregister") } : new FinalizationRegistry(({ ptr: n, instance: t }) => {
  t === o && i.__wbg_intounderlyingsource_free(n >>> 0, 1);
});
var it = typeof FinalizationRegistry > "u" ? { register: /* @__PURE__ */ __name(() => {
}, "register"), unregister: /* @__PURE__ */ __name(() => {
}, "unregister") } : new FinalizationRegistry(({ ptr: n, instance: t }) => {
  t === o && i.__wbg_minifyconfig_free(n >>> 0, 1);
});
var ot = typeof FinalizationRegistry > "u" ? { register: /* @__PURE__ */ __name(() => {
}, "register"), unregister: /* @__PURE__ */ __name(() => {
}, "unregister") } : new FinalizationRegistry(({ ptr: n, instance: t }) => {
  t === o && i.__wbg_r2range_free(n >>> 0, 1);
});
function l(n) {
  let t = i.__externref_table_alloc();
  return i.__wbindgen_externrefs.set(t, n), t;
}
__name(l, "l");
var q = typeof FinalizationRegistry > "u" ? { register: /* @__PURE__ */ __name(() => {
}, "register"), unregister: /* @__PURE__ */ __name(() => {
}, "unregister") } : new FinalizationRegistry((n) => {
  n.instance === o && n.dtor(n.a, n.b);
});
function M(n) {
  let t = typeof n;
  if (t == "number" || t == "boolean" || n == null) return `${n}`;
  if (t == "string") return `"${n}"`;
  if (t == "symbol") {
    let _ = n.description;
    return _ == null ? "Symbol" : `Symbol(${_})`;
  }
  if (t == "function") {
    let _ = n.name;
    return typeof _ == "string" && _.length > 0 ? `Function(${_})` : "Function";
  }
  if (Array.isArray(n)) {
    let _ = n.length, s = "[";
    _ > 0 && (s += M(n[0]));
    for (let c = 1; c < _; c++) s += ", " + M(n[c]);
    return s += "]", s;
  }
  let e = /\[object ([^\]]+)\]/.exec(toString.call(n)), r;
  if (e && e.length > 1) r = e[1];
  else return toString.call(n);
  if (r == "Object") try {
    return "Object(" + JSON.stringify(n) + ")";
  } catch {
    return "Object";
  }
  return n instanceof Error ? `${n.name}: ${n.message}
${n.stack}` : r;
}
__name(M, "M");
function st(n, t) {
  n = n >>> 0;
  let e = b(), r = [];
  for (let _ = n; _ < n + 4 * t; _ += 4) r.push(i.__wbindgen_externrefs.get(e.getUint32(_, true)));
  return i.__externref_drop_slice(n, t), r;
}
__name(st, "st");
function A(n, t) {
  return n = n >>> 0, j().subarray(n / 1, n / 1 + t);
}
__name(A, "A");
var p = null;
function b() {
  return (p === null || p.buffer.detached === true || p.buffer.detached === void 0 && p.buffer !== i.memory.buffer) && (p = new DataView(i.memory.buffer)), p;
}
__name(b, "b");
function w(n, t) {
  return n = n >>> 0, ft(n, t);
}
__name(w, "w");
var W = null;
function j() {
  return (W === null || W.byteLength === 0) && (W = new Uint8Array(i.memory.buffer)), W;
}
__name(j, "j");
function u(n, t) {
  try {
    return n.apply(this, t);
  } catch (e) {
    let r = l(e);
    i.__wbindgen_exn_store(r);
  }
}
__name(u, "u");
function a(n) {
  return n == null;
}
__name(a, "a");
function D(n, t, e, r) {
  let _ = { a: n, b: t, cnt: 1, dtor: e, instance: o }, s = /* @__PURE__ */ __name((...c) => {
    if (_.instance !== o) throw new Error("Cannot invoke closure from previous WASM instance");
    _.cnt++;
    let f = _.a;
    _.a = 0;
    try {
      return r(f, _.b, ...c);
    } finally {
      _.a = f, s._wbg_cb_unref();
    }
  }, "s");
  return s._wbg_cb_unref = () => {
    --_.cnt === 0 && (_.dtor(_.a, _.b), _.a = 0, q.unregister(_));
  }, q.register(s, _, _), s;
}
__name(D, "D");
function ct(n, t) {
  let e = t(n.length * 4, 4) >>> 0;
  for (let r = 0; r < n.length; r++) {
    let _ = l(n[r]);
    b().setUint32(e + 4 * r, _, true);
  }
  return d = n.length, e;
}
__name(ct, "ct");
function y(n, t, e) {
  if (e === void 0) {
    let f = F.encode(n), g = t(f.length, 1) >>> 0;
    return j().subarray(g, g + f.length).set(f), d = f.length, g;
  }
  let r = n.length, _ = t(r, 1) >>> 0, s = j(), c = 0;
  for (; c < r; c++) {
    let f = n.charCodeAt(c);
    if (f > 127) break;
    s[_ + c] = f;
  }
  if (c !== r) {
    c !== 0 && (n = n.slice(c)), _ = e(_, r, r = c + n.length * 3, 1) >>> 0;
    let f = j().subarray(_ + c, _ + r), g = F.encodeInto(n, f);
    c += g.written, _ = e(_, r, c, 1) >>> 0;
  }
  return d = c, _;
}
__name(y, "y");
function ut(n) {
  let t = i.__wbindgen_externrefs.get(n);
  return i.__externref_table_dealloc(n), t;
}
__name(ut, "ut");
var $ = new TextDecoder("utf-8", { ignoreBOM: true, fatal: true });
$.decode();
function ft(n, t) {
  return $.decode(j().subarray(n, n + t));
}
__name(ft, "ft");
var F = new TextEncoder();
"encodeInto" in F || (F.encodeInto = function(n, t) {
  let e = F.encode(n);
  return t.set(e), { read: n.length, written: e.length };
});
var d = 0;
var at = new WebAssembly.Instance(N, B());
var i = at.exports;
i.__wbindgen_start();
Error.stackTraceLimit = 100;
var k = false;
function J() {
  C && C(function(n) {
    let t = new Error("Rust panic: " + n);
    console.error("Critical", t), k = true;
  });
}
__name(J, "J");
J();
var P = 0;
function U() {
  k && (console.log("Reinitializing Wasm application"), H(), k = false, J(), P++);
}
__name(U, "U");
addEventListener("error", (n) => {
  L(n.error);
});
function L(n) {
  n instanceof WebAssembly.RuntimeError && (console.error("Critical", n), k = true);
}
__name(L, "L");
var z = class extends gt {
  static {
    __name(this, "z");
  }
};
z.prototype.fetch = function(t) {
  return V.call(this, t, this.env, this.ctx);
};
var dt = { set: /* @__PURE__ */ __name((n, t, e, r) => Reflect.set(n.instance, t, e, r), "set"), has: /* @__PURE__ */ __name((n, t) => Reflect.has(n.instance, t), "has"), deleteProperty: /* @__PURE__ */ __name((n, t) => Reflect.deleteProperty(n.instance, t), "deleteProperty"), apply: /* @__PURE__ */ __name((n, t, e) => Reflect.apply(n.instance, t, e), "apply"), construct: /* @__PURE__ */ __name((n, t, e) => Reflect.construct(n.instance, t, e), "construct"), getPrototypeOf: /* @__PURE__ */ __name((n) => Reflect.getPrototypeOf(n.instance), "getPrototypeOf"), setPrototypeOf: /* @__PURE__ */ __name((n, t) => Reflect.setPrototypeOf(n.instance, t), "setPrototypeOf"), isExtensible: /* @__PURE__ */ __name((n) => Reflect.isExtensible(n.instance), "isExtensible"), preventExtensions: /* @__PURE__ */ __name((n) => Reflect.preventExtensions(n.instance), "preventExtensions"), getOwnPropertyDescriptor: /* @__PURE__ */ __name((n, t) => Reflect.getOwnPropertyDescriptor(n.instance, t), "getOwnPropertyDescriptor"), defineProperty: /* @__PURE__ */ __name((n, t, e) => Reflect.defineProperty(n.instance, t, e), "defineProperty"), ownKeys: /* @__PURE__ */ __name((n) => Reflect.ownKeys(n.instance), "ownKeys") };
var h = { construct(n, t, e) {
  try {
    U();
    let r = { instance: Reflect.construct(n, t, e), instanceId: P, ctor: n, args: t, newTarget: e };
    return new Proxy(r, { ...dt, get(_, s, c) {
      _.instanceId !== P && (_.instance = Reflect.construct(_.ctor, _.args, _.newTarget), _.instanceId = P);
      let f = Reflect.get(_.instance, s, c);
      return typeof f != "function" ? f : f.constructor === Function ? new Proxy(f, { apply(g, T, O) {
        U();
        try {
          return g.apply(T, O);
        } catch (S) {
          throw L(S), S;
        }
      } }) : new Proxy(f, { async apply(g, T, O) {
        U();
        try {
          return await g.apply(T, O);
        } catch (S) {
          throw L(S), S;
        }
      } });
    } });
  } catch (r) {
    throw k = true, r;
  }
} };
var pt = new Proxy(z, h);
var ht = new Proxy(m, h);
var yt = new Proxy(x, h);
var mt = new Proxy(v, h);
var xt = new Proxy(I, h);
var vt = new Proxy(R, h);
var It = new Proxy(E, h);

// ../../../npm-packages/lib/node_modules/wrangler/templates/middleware/middleware-ensure-req-body-drained.ts
var drainBody = /* @__PURE__ */ __name(async (request, env, _ctx, middlewareCtx) => {
  try {
    return await middlewareCtx.next(request, env);
  } finally {
    try {
      if (request.body !== null && !request.bodyUsed) {
        const reader = request.body.getReader();
        while (!(await reader.read()).done) {
        }
      }
    } catch (e) {
      console.error("Failed to drain the unused request body.", e);
    }
  }
}, "drainBody");
var middleware_ensure_req_body_drained_default = drainBody;

// ../../../npm-packages/lib/node_modules/wrangler/templates/middleware/middleware-miniflare3-json-error.ts
function reduceError(e) {
  return {
    name: e?.name,
    message: e?.message ?? String(e),
    stack: e?.stack,
    cause: e?.cause === void 0 ? void 0 : reduceError(e.cause)
  };
}
__name(reduceError, "reduceError");
var jsonError = /* @__PURE__ */ __name(async (request, env, _ctx, middlewareCtx) => {
  try {
    return await middlewareCtx.next(request, env);
  } catch (e) {
    const error = reduceError(e);
    return Response.json(error, {
      status: 500,
      headers: { "MF-Experimental-Error-Stack": "true" }
    });
  }
}, "jsonError");
var middleware_miniflare3_json_error_default = jsonError;

// .wrangler/tmp/bundle-HgbPzV/middleware-insertion-facade.js
var __INTERNAL_WRANGLER_MIDDLEWARE__ = [
  middleware_ensure_req_body_drained_default,
  middleware_miniflare3_json_error_default
];
var middleware_insertion_facade_default = pt;

// ../../../npm-packages/lib/node_modules/wrangler/templates/middleware/common.ts
var __facade_middleware__ = [];
function __facade_register__(...args) {
  __facade_middleware__.push(...args.flat());
}
__name(__facade_register__, "__facade_register__");
function __facade_invokeChain__(request, env, ctx, dispatch, middlewareChain) {
  const [head, ...tail] = middlewareChain;
  const middlewareCtx = {
    dispatch,
    next(newRequest, newEnv) {
      return __facade_invokeChain__(newRequest, newEnv, ctx, dispatch, tail);
    }
  };
  return head(request, env, ctx, middlewareCtx);
}
__name(__facade_invokeChain__, "__facade_invokeChain__");
function __facade_invoke__(request, env, ctx, dispatch, finalMiddleware) {
  return __facade_invokeChain__(request, env, ctx, dispatch, [
    ...__facade_middleware__,
    finalMiddleware
  ]);
}
__name(__facade_invoke__, "__facade_invoke__");

// .wrangler/tmp/bundle-HgbPzV/middleware-loader.entry.ts
var __Facade_ScheduledController__ = class ___Facade_ScheduledController__ {
  constructor(scheduledTime, cron, noRetry) {
    this.scheduledTime = scheduledTime;
    this.cron = cron;
    this.#noRetry = noRetry;
  }
  static {
    __name(this, "__Facade_ScheduledController__");
  }
  #noRetry;
  noRetry() {
    if (!(this instanceof ___Facade_ScheduledController__)) {
      throw new TypeError("Illegal invocation");
    }
    this.#noRetry();
  }
};
function wrapExportedHandler(worker) {
  if (__INTERNAL_WRANGLER_MIDDLEWARE__ === void 0 || __INTERNAL_WRANGLER_MIDDLEWARE__.length === 0) {
    return worker;
  }
  for (const middleware of __INTERNAL_WRANGLER_MIDDLEWARE__) {
    __facade_register__(middleware);
  }
  const fetchDispatcher = /* @__PURE__ */ __name(function(request, env, ctx) {
    if (worker.fetch === void 0) {
      throw new Error("Handler does not export a fetch() function.");
    }
    return worker.fetch(request, env, ctx);
  }, "fetchDispatcher");
  return {
    ...worker,
    fetch(request, env, ctx) {
      const dispatcher = /* @__PURE__ */ __name(function(type, init) {
        if (type === "scheduled" && worker.scheduled !== void 0) {
          const controller = new __Facade_ScheduledController__(
            Date.now(),
            init.cron ?? "",
            () => {
            }
          );
          return worker.scheduled(controller, env, ctx);
        }
      }, "dispatcher");
      return __facade_invoke__(request, env, ctx, dispatcher, fetchDispatcher);
    }
  };
}
__name(wrapExportedHandler, "wrapExportedHandler");
function wrapWorkerEntrypoint(klass) {
  if (__INTERNAL_WRANGLER_MIDDLEWARE__ === void 0 || __INTERNAL_WRANGLER_MIDDLEWARE__.length === 0) {
    return klass;
  }
  for (const middleware of __INTERNAL_WRANGLER_MIDDLEWARE__) {
    __facade_register__(middleware);
  }
  return class extends klass {
    #fetchDispatcher = /* @__PURE__ */ __name((request, env, ctx) => {
      this.env = env;
      this.ctx = ctx;
      if (super.fetch === void 0) {
        throw new Error("Entrypoint class does not define a fetch() function.");
      }
      return super.fetch(request);
    }, "#fetchDispatcher");
    #dispatcher = /* @__PURE__ */ __name((type, init) => {
      if (type === "scheduled" && super.scheduled !== void 0) {
        const controller = new __Facade_ScheduledController__(
          Date.now(),
          init.cron ?? "",
          () => {
          }
        );
        return super.scheduled(controller);
      }
    }, "#dispatcher");
    fetch(request) {
      return __facade_invoke__(
        request,
        this.env,
        this.ctx,
        this.#dispatcher,
        this.#fetchDispatcher
      );
    }
  };
}
__name(wrapWorkerEntrypoint, "wrapWorkerEntrypoint");
var WRAPPED_ENTRY;
if (typeof middleware_insertion_facade_default === "object") {
  WRAPPED_ENTRY = wrapExportedHandler(middleware_insertion_facade_default);
} else if (typeof middleware_insertion_facade_default === "function") {
  WRAPPED_ENTRY = wrapWorkerEntrypoint(middleware_insertion_facade_default);
}
var middleware_loader_entry_default = WRAPPED_ENTRY;
export {
  ht as ContainerStartupOptions,
  yt as IntoUnderlyingByteSource,
  mt as IntoUnderlyingSink,
  xt as IntoUnderlyingSource,
  vt as MinifyConfig,
  It as R2Range,
  __INTERNAL_WRANGLER_MIDDLEWARE__,
  middleware_loader_entry_default as default
};
//# sourceMappingURL=shim.js.map
