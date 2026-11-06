//! The latest browser AutoSave draft, kept in IndexedDB and restored on the next visit.

use wasm_bindgen::JsCast as _;
use wasm_bindgen::JsValue;
use wasm_bindgen::closure::Closure;
use web_sys::{IdbDatabase, IdbRequest, IdbTransaction, IdbTransactionMode};
use wordcraft_ui_egui::async_saves::{AsyncSaves, SaveOutcome};

const DB_NAME: &str = "wordcraft";
const DB_VERSION: u32 = 2;
const STORE: &str = "autosaves";
const KEY: &str = "latest";
const MAX_DRAFT_BYTES: usize = 256 << 20;

fn js(error: JsValue) -> String {
    format!("{error:?}")
}

async fn request_result(request: &IdbRequest) -> Result<JsValue, String> {
    let promise = js_sys::Promise::new(&mut |resolve: js_sys::Function, reject: js_sys::Function| {
        let success_request = request.clone();
        let success = Closure::once_into_js(move || {
            let _ = resolve.call1(&JsValue::NULL, &success_request.result().unwrap_or(JsValue::UNDEFINED));
        });
        let failure = Closure::once_into_js(move || {
            let _ = reject.call1(&JsValue::NULL, &JsValue::from_str("IndexedDB request failed"));
        });
        request.set_onsuccess(Some(success.unchecked_ref()));
        request.set_onerror(Some(failure.unchecked_ref()));
    });
    wasm_bindgen_futures::JsFuture::from(promise).await.map_err(js)
}

fn transaction_committed(transaction: &IdbTransaction) -> js_sys::Promise {
    js_sys::Promise::new(&mut |resolve: js_sys::Function, reject: js_sys::Function| {
        let reject_abort = reject.clone();
        let reject_error = reject.clone();
        let success = Closure::once_into_js(move || {
            let _ = resolve.call0(&JsValue::NULL);
        });
        let abort = Closure::once_into_js(move || {
            let _ = reject_abort.call1(&JsValue::NULL, &JsValue::from_str("IndexedDB transaction failed"));
        });
        let error = Closure::once_into_js(move || {
            let _ = reject_error.call1(&JsValue::NULL, &JsValue::from_str("IndexedDB transaction failed"));
        });
        transaction.set_oncomplete(Some(success.unchecked_ref()));
        transaction.set_onabort(Some(abort.unchecked_ref()));
        transaction.set_onerror(Some(error.unchecked_ref()));
    })
}

async fn open() -> Result<IdbDatabase, String> {
    let factory = web_sys::window().ok_or("no window")?.indexed_db().map_err(js)?.ok_or("IndexedDB is unavailable")?;
    let request = factory.open_with_u32(DB_NAME, DB_VERSION).map_err(js)?;
    let upgrade_request = request.clone();
    let upgrade = Closure::<dyn FnMut()>::new(move || {
        if let Ok(database) = upgrade_request.result().and_then(|result| result.dyn_into::<IdbDatabase>())
            && !database.object_store_names().contains("fonts")
            && let Err(error) = database.create_object_store("fonts")
        {
            log::error!("couldn't make the font store: {error:?}");
        }
        if let Ok(database) = upgrade_request.result().and_then(|result| result.dyn_into::<IdbDatabase>())
            && !database.object_store_names().contains(STORE)
            && let Err(error) = database.create_object_store(STORE)
        {
            log::error!("couldn't make the autosave store: {error:?}");
        }
    });
    request.set_onupgradeneeded(Some(upgrade.as_ref().unchecked_ref()));
    let result = request_result(&request).await;
    request.set_onupgradeneeded(None);
    drop(upgrade);
    result?.dyn_into::<IdbDatabase>().map_err(js)
}

async fn store(bytes: Vec<u8>) -> Result<(), String> {
    if bytes.len() > MAX_DRAFT_BYTES {
        return Err(format!("the AutoSave draft exceeds {} MiB", MAX_DRAFT_BYTES >> 20));
    }
    let database = open().await?;
    let result = async {
        let transaction = database.transaction_with_str_and_mode(STORE, IdbTransactionMode::Readwrite).map_err(js)?;
        let committed = transaction_committed(&transaction);
        let object_store = transaction.object_store(STORE).map_err(js)?;
        let request = object_store
            .put_with_key(&js_sys::Uint8Array::from(bytes.as_slice()), &JsValue::from_str(KEY))
            .map_err(js)?;
        request_result(&request).await?;
        wasm_bindgen_futures::JsFuture::from(committed).await.map(|_| ()).map_err(js)
    }
    .await;
    database.close();
    result
}

async fn load() -> Result<Option<Vec<u8>>, String> {
    let database = open().await?;
    let result = async {
        let transaction = database.transaction_with_str(STORE).map_err(js)?;
        let object_store = transaction.object_store(STORE).map_err(js)?;
        let value = request_result(&object_store.get(&JsValue::from_str(KEY)).map_err(js)?).await?;
        if value.is_undefined() || value.is_null() {
            return Ok(None);
        }
        let bytes = value.dyn_into::<js_sys::Uint8Array>().map_err(js)?;
        if bytes.length() as usize > MAX_DRAFT_BYTES {
            return Err("the stored AutoSave draft exceeds the size limit".into());
        }
        Ok(Some(bytes.to_vec()))
    }
    .await;
    database.close();
    result
}

async fn clear() -> Result<(), String> {
    let database = open().await?;
    let result = async {
        let transaction = database.transaction_with_str_and_mode(STORE, IdbTransactionMode::Readwrite).map_err(js)?;
        let committed = transaction_committed(&transaction);
        let object_store = transaction.object_store(STORE).map_err(js)?;
        let request = object_store.delete(&JsValue::from_str(KEY)).map_err(js)?;
        request_result(&request).await?;
        wasm_bindgen_futures::JsFuture::from(committed).await.map(|_| ()).map_err(js)
    }
    .await;
    database.close();
    result
}

/// Begin an AutoSave write and return its unload-protection ticket.
pub fn start_save(name: String, bytes: Vec<u8>, saves: AsyncSaves, ctx: egui::Context) -> Result<u64, String> {
    if bytes.len() > MAX_DRAFT_BYTES {
        return Err(format!("the AutoSave draft exceeds {} MiB", MAX_DRAFT_BYTES >> 20));
    }
    let ticket = saves.start_background();
    wasm_bindgen_futures::spawn_local(async move {
        let outcome = match store(bytes).await {
            Ok(()) => SaveOutcome::Saved,
            Err(error) => SaveOutcome::Failed(error),
        };
        saves.finish(ticket, outcome);
        ctx.request_repaint();
    });
    log::debug!("started browser AutoSave for {name}");
    Ok(ticket)
}

/// Load the last autosaved document, if this browser has one.
pub async fn load_draft() -> Result<Option<Vec<u8>>, String> {
    load().await
}

/// Remove the stored draft after a manual save or when AutoSave is switched off.
pub fn clear_draft() {
    wasm_bindgen_futures::spawn_local(async {
        if let Err(error) = clear().await {
            log::warn!("couldn't clear the browser AutoSave draft: {error}");
        }
    });
}