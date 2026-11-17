use crate::js::*;
use std::{cell::RefCell, rc::Rc};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::spawn_local;

/// Base intervals; failures back off exponentially up to `MAX_BACKOFF_MS`.
const STATUS_MS: f64 = 1000.0;
const SCREEN_MS: f64 = 1800.0;
const MAX_BACKOFF_MS: f64 = 30000.0;

/// Index 0 is the status loop, index 1 the (Android) screenshot loop.
fn slot(preview: bool) -> usize {
    usize::from(preview)
}
struct Polling {
    client: JsValue,
    options: JsValue,
    session: JsValue,
    stopped: bool,
    timers: Vec<JsValue>,
    /// Consecutive failures per loop; reset on the first success.
    failures: [u32; 2],
    /// A loop that found the tab hidden waits for `visibilitychange` instead of a timer.
    paused: [bool; 2],
    document: JsValue,
    on_visibility: JsValue,
}
impl Polling {
    fn session(&self) -> JsValue {
        get(&call(&self.client, "snapshot", &[]).unwrap_or_default(), "session")
    }
    fn current_for(&self, session: &JsValue) -> bool {
        !self.stopped && get(session, "id") == self.session
    }
    fn current(&self) -> bool {
        self.current_for(&self.session())
    }
    fn ready(&self) -> bool {
        let session = self.session();
        self.current_for(&session)
            && truth(&get(&session, "ready"))
            && text(&get(&get(&session, "daemon"), "state"), "state") == "ready"
    }
    fn hidden(&self) -> bool {
        truth(&get(&self.document, "hidden"))
    }
    fn delay(&self, preview: bool) -> f64 {
        let base = if preview { SCREEN_MS } else { STATUS_MS };
        match self.failures[slot(preview)] {
            0 => base,
            // 1 s, 2 s, 4 s ... capped at 30 s.
            n => (STATUS_MS * 2f64.powi(n.min(16) as i32 - 1)).max(base).min(MAX_BACKOFF_MS),
        }
    }
}
fn later(state: Rc<RefCell<Polling>>, preview: bool) {
    if !state.borrow().current() {
        return;
    }
    if state.borrow().hidden() {
        // Background tabs do not poll; `visibilitychange` resumes immediately.
        state.borrow_mut().paused[slot(preview)] = true;
        return;
    }
    let weak = Rc::downgrade(&state);
    let timer_id = Rc::new(RefCell::new(JsValue::UNDEFINED));
    let callback_id = timer_id.clone();
    let timer = callback(move |_| {
        if let Some(state) = weak.upgrade() {
            state.borrow_mut().timers.retain(|id| id != &*callback_id.borrow());
            tick(state, preview);
        }
        JsValue::UNDEFINED
    });
    let schedule = get(&state.borrow().options, "schedule");
    let schedule = if schedule.is_function() { schedule } else { get(&global(), "setTimeout") };
    let delay = state.borrow().delay(preview);
    let id = apply(&schedule, &global(), &[timer, delay.into()]).unwrap_or_default();
    *timer_id.borrow_mut() = id.clone();
    state.borrow_mut().timers.push(id);
}
fn tick(state: Rc<RefCell<Polling>>, preview: bool) {
    if state.borrow().hidden() {
        // A timer that fires after the tab was hidden does not issue a request.
        later(state, preview);
        return;
    }
    let client = state.borrow().client.clone();
    let request = if preview {
        let active = get(&state.borrow().options, "previewActive");
        if active.is_function() && apply(&active, &JsValue::UNDEFINED, &[]).is_ok_and(|v| truth(&v)) {
            later(state, preview);
            return;
        }
        if state.borrow().ready() {
            call(&client, "screen", &[])
        } else {
            later(state, preview);
            return;
        }
    } else {
        call(&client, "refresh", &[])
    };
    spawn_local(async move {
        let result = match request {
            Ok(v) => resolved(v).await,
            Err(e) => Err(e),
        };
        let options = state.borrow().options.clone();
        match result {
            Ok(image) => {
                state.borrow_mut().failures[slot(preview)] = 0;
                if preview {
                    let device = get(&get(&call(&client, "snapshot", &[]).unwrap_or_default(), "session"), "deviceId");
                    if state.borrow().ready() && get(&image, "deviceId") == device {
                        let _ = apply(&get(&options, "onScreen"), &JsValue::UNDEFINED, &[image]);
                    }
                } else if state.borrow().current() && !state.borrow().ready() {
                    let _ = apply(&get(&options, "onScreen"), &JsValue::UNDEFINED, &[JsValue::NULL]);
                }
            }
            Err(e) => {
                {
                    let mut s = state.borrow_mut();
                    s.failures[slot(preview)] = s.failures[slot(preview)].saturating_add(1);
                }
                if state.borrow().current() {
                    let _ = apply(&get(&options, "onError"), &JsValue::UNDEFINED, &[message(&e).into()]);
                }
            }
        }
        later(state, preview);
    });
}
fn listen(document: &JsValue, method: &str, listener: &JsValue) {
    if get(document, method).is_function() {
        let _ = call(document, method, &["visibilitychange".into(), listener.clone()]);
    }
}
#[wasm_bindgen(js_name=startSessionPolling)]
pub fn start_session_polling(client: JsValue, options: JsValue) -> JsValue {
    let session = get(&get(&call(&client, "snapshot", &[]).unwrap_or_default(), "session"), "id");
    // `options.document` lets tests drive visibility; browsers use the page document.
    let document = match get(&options, "document") {
        d if d.is_undefined() => get(&global(), "document"),
        d => d,
    };
    let state = Rc::new(RefCell::new(Polling {
        client,
        options,
        session,
        stopped: false,
        timers: vec![],
        failures: [0; 2],
        paused: [false; 2],
        document: document.clone(),
        on_visibility: JsValue::UNDEFINED,
    }));
    let weak = Rc::downgrade(&state);
    let on_visibility = callback(move |_| {
        let Some(state) = weak.upgrade() else { return JsValue::UNDEFINED };
        if state.borrow().stopped || state.borrow().hidden() {
            return JsValue::UNDEFINED;
        }
        let resume = std::mem::take(&mut state.borrow_mut().paused);
        for (index, paused) in resume.into_iter().enumerate() {
            if paused {
                tick(state.clone(), index == 1);
            }
        }
        JsValue::UNDEFINED
    });
    listen(&document, "addEventListener", &on_visibility);
    state.borrow_mut().on_visibility = on_visibility;
    tick(state.clone(), false);
    if truth(&get(&state.borrow().options, "screenshots")) {
        tick(state.clone(), true);
    }
    callback(move |_| {
        let (timers, document, listener) = {
            let mut s = state.borrow_mut();
            s.stopped = true;
            s.paused = [false; 2];
            (std::mem::take(&mut s.timers), s.document.clone(), s.on_visibility.clone())
        };
        listen(&document, "removeEventListener", &listener);
        let unschedule = get(&state.borrow().options, "unschedule");
        let unschedule = if unschedule.is_function() { unschedule } else { get(&global(), "clearTimeout") };
        for timer in timers {
            let _ = apply(&unschedule, &global(), &[timer]);
        }
        JsValue::UNDEFINED
    })
}
