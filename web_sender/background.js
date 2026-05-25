/*
 Service worker entry of the Hourglass Web Sensor extension.

 We do not pipe every Chrome event straight to the native host; instead we
 fold them into a single (focused, window_id, tab_id, url) identity tuple
 and only emit when that tuple flips.
 */

const STR_NATIVE_HOST = "com.hourglass.web_receiver";
const DEBOUNCE_MS = 120;
const WINDOW_ID_NONE = -1;

const EVENT_ON_START      = "on_start";
const EVENT_FOCUS_GAINED  = "focus_gained";
const EVENT_TAB_ACTIVATED = "tab_activated";
const EVENT_URL_CHANGED   = "url_changed";
const EVENT_PAGE_LOADED   = "page_loaded";
const EVENT_TAB_REPLACED  = "tab_replaced";

// information of current active tab
const state = {
    flag_focused: false,
    window_id: null,
    tab_id: null,
    str_url: null,
    str_title: null,
};

let str_last_sent_key = null;  // a string with "${flag_focused}|${window_id}|${tab_id}|${str_url}" to eliminate tab refresh jitter
let str_pending_event = null;  // staged event
let debounce_timer = null;
let native_port = null;


function connect_native() {
    if (native_port) return native_port;
    try {
        const port = chrome.runtime.connectNative(STR_NATIVE_HOST);
        port.onDisconnect.addListener(() => {
            // lastError must be read inside the callback, or it gets cleared.
            const err = chrome.runtime.lastError;
            if (err) console.warn("[hourglass] native port disconnected:", err.message);
            native_port = null;
            str_last_sent_key = null;
        });
        // Receiver is one-way for now; drain inbound to keep the port healthy.
        port.onMessage.addListener(() => { });
        native_port = port;
    } catch (e) {
        console.warn("[hourglass] connectNative failed:", e);
        native_port = null;
    }
    return native_port;
}

function snapshot_key(flag_focused, window_id, tab_id, str_url) {
    return `${flag_focused}|${window_id}|${tab_id}|${str_url}`;
}

function build_payload() {
    if (!state.flag_focused || state.tab_id === null) {  // backstage or no tab
        return {
            focused: false,
            window_id: null,
            tab_id: null,
            url: null,
            title: null,
        };
    }
    return {
        focused: true,
        window_id: state.window_id,
        tab_id: state.tab_id,
        url: state.str_url || "",
        title: state.str_title || "",
    };
}

function schedule_send(str_event) {
    // the latest reason replaces any earlier one due to the possible event burst
    str_pending_event = str_event;
    if (debounce_timer !== null) clearTimeout(debounce_timer);
    debounce_timer = setTimeout(flush_send, DEBOUNCE_MS);
}

function flush_send() {
    debounce_timer = null;

    const str_event = str_pending_event;
    str_pending_event = null;
    if (!str_event) return;  // guard against a stray flush with no event tag staged

    const payload = build_payload();
    const str_key = snapshot_key(payload.focused, payload.window_id, payload.tab_id, payload.url);
    if (str_last_sent_key === str_key) return;  // no actual change

    const port = connect_native();
    if (!port) return;

    const message = { event: str_event, ...payload, timestamp_ms: Date.now() };
    try {
        port.postMessage(message);
        str_last_sent_key = str_key;
    } catch (e) {
        // postMessage throws synchronously when the port is already half-closed
        // drop refs and clean port info so the next event reconnects from scratch.
        console.warn("[hourglass] postMessage failed:", e);
        native_port = null;
        str_last_sent_key = null;
    }
}

function clear_focus() {
    state.flag_focused = false;
    state.window_id = null;
    state.tab_id = null;
    state.str_url = null;
    state.str_title = null;
}

function adopt_tab(tab) {
    state.flag_focused = true;
    state.window_id = tab.windowId;
    state.tab_id = tab.id;
    state.str_url = tab.url || tab.pendingUrl || "";  // pendingUrl for committed url being attached
    state.str_title = tab.title || "";
}

async function refresh_from_window(window_id, str_event) {
    // callers guarantee window_id refers to a real, focused window
    try {
        const vec_tabs = await chrome.tabs.query({ active: true, windowId: window_id });
        if (!vec_tabs.length) {  // window exists but has no active tab record yet (e.g., still booting)
            clear_focus();
            return;
        }
        adopt_tab(vec_tabs[0]);
        schedule_send(str_event);
    } catch (e) {
        console.warn("[hourglass] tabs.query failed:", e);
        clear_focus();
    }
}

// focus changed
chrome.windows.onFocusChanged.addListener((window_id) => {
    if (window_id === WINDOW_ID_NONE) {  // lose focus
        clear_focus();
        return;
    }
    refresh_from_window(window_id, EVENT_FOCUS_GAINED).then(_ => {});
});

// active tab change inside a window
// ignore it when onActivated happens on backstage
chrome.tabs.onActivated.addListener(({ tabId, windowId }) => {
    if (state.flag_focused && state.window_id !== null && windowId !== state.window_id) return;
    chrome.tabs.get(tabId).then((tab) => {
        adopt_tab(tab);
        schedule_send(EVENT_TAB_ACTIVATED);
    }).catch((e) => {
        console.warn("[hourglass] tabs.get failed:", e);
    });
});

// tab metadata updates from the currently viewed tab
chrome.tabs.onUpdated.addListener((tab_id, change_info, tab) => {
    if (!state.flag_focused) return;
    if (tab_id !== state.tab_id) return;

    let str_event = null;

    if (typeof change_info.url === "string") {
        state.str_url = change_info.url;
        str_event = EVENT_URL_CHANGED;
    }
    if (typeof change_info.title === "string") {
        state.str_title = change_info.title;
    }
    if (change_info.status === "complete") {
        // The committed url may differ from any intermediate one we already saw.
        if (tab.url && tab.url !== state.str_url) state.str_url = tab.url;
        if (tab.title) state.str_title = tab.title;
        // page_loaded is more informative than url_changed when both apply inside the same callback
        str_event = EVENT_PAGE_LOADED;
    }

    if (str_event) schedule_send(str_event);
});

// force upload when tab replaced by another
chrome.tabs.onReplaced.addListener((added_tab_id, removed_tab_id) => {
    if (state.tab_id !== removed_tab_id) return;
    chrome.tabs.get(added_tab_id).then((tab) => {
        adopt_tab(tab);
        schedule_send(EVENT_TAB_REPLACED);
    }).catch(() => { });
});

// clear the state info and waiting for next onActivated
chrome.tabs.onRemoved.addListener((tab_id, _remove_info) => {
    if (state.tab_id !== tab_id) return;
    state.tab_id = null;
    state.str_url = null;
    state.str_title = null;
});

// clear everything up
chrome.windows.onRemoved.addListener((window_id) => {
    if (state.window_id !== window_id) return;
    clear_focus();
});

async function bootstrap() {
    // service workers can be respawned by any event with cleaning memory
    // try to get last focus info when the extension comes alive (and compare to the current info)
    try {
        const win = await chrome.windows.getLastFocused({ populate: false });
        if (win && win.focused) {
            await refresh_from_window(win.id, EVENT_ON_START);
        } else {
            // no browser window (if)
            clear_focus();
        }
    } catch (e) {
        console.warn("[hourglass] bootstrap failed:", e);
    }
}

chrome.runtime.onStartup.addListener(bootstrap);
chrome.runtime.onInstalled.addListener(bootstrap);
bootstrap().then(_ => { });
