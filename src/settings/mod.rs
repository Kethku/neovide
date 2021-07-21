use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::convert::TryInto;

mod from_value;
mod window_geometry;
pub use from_value::FromValue;
use log::trace;
use nvim_rs::Neovim;
use parking_lot::RwLock;
pub use rmpv::Value;
pub use window_geometry::{
    maybe_save_window_size, parse_window_geometry, WindowGeometry, DEFAULT_WINDOW_GEOMETRY,
};

use crate::bridge::TxWrapper;
use crate::error_handling::ResultPanicExplanation;

lazy_static! {
    pub static ref SETTINGS: Settings = Settings::new();
}

pub trait SettingGroup {
    fn register(&self);
}

// Function types to handle settings updates
type UpdateHandlerFunc = fn(Value);
type ReaderFunc = fn() -> Value;

// The Settings struct acts as a global container where each of Neovide's subsystems can store
// their own settings. It will also coordinate updates between Neovide and nvim to make sure the
// settings remain consistent on both sides.
// Note: As right now we're only sending new setting values to Neovide during the
// read_initial_values call, after that point we should not modify the contents of the Settings
// struct except when prompted by an update event from nvim. Otherwise, the settings in Neovide and
// nvim will get out of sync.
pub struct Settings {
    settings: RwLock<HashMap<TypeId, Box<dyn Any + Send + Sync>>>,
    listeners: RwLock<HashMap<String, UpdateHandlerFunc>>,
    readers: RwLock<HashMap<String, ReaderFunc>>,
}

impl Settings {
    fn new() -> Self {
        Self {
            settings: RwLock::new(HashMap::new()),
            listeners: RwLock::new(HashMap::new()),
            readers: RwLock::new(HashMap::new()),
        }
    }

    pub fn set_setting_handlers(
        &self,
        property_name: &str,
        update_func: UpdateHandlerFunc,
        reader_func: ReaderFunc,
    ) {
        self.listeners
            .write()
            .insert(String::from(property_name), update_func);
        self.readers
            .write()
            .insert(String::from(property_name), reader_func);
    }

    pub fn set<T: Clone + Send + Sync + 'static>(&self, t: &T) {
        let type_id: TypeId = TypeId::of::<T>();
        let t: T = (*t).clone();
        unsafe {
            self.settings.force_unlock_write();
        }
        let mut write_lock = self.settings.write();
        write_lock.insert(type_id, Box::new(t));
    }

    pub fn get<T: Clone + Send + Sync + 'static>(&'_ self) -> T {
        let read_lock = self.settings.read();
        let boxed = &read_lock
            .get(&TypeId::of::<T>())
            .expect("Trying to retrieve a settings object that doesn't exist: {:?}");
        let value: &T = boxed
            .downcast_ref::<T>()
            .expect("Attempted to extract a settings object of the wrong type");
        (*value).clone()
    }

    pub async fn read_initial_values(&self, nvim: &Neovim<TxWrapper>) {
        let keys: Vec<String> = self.listeners.read().keys().cloned().collect();

        for name in keys {
            let variable_name = format!("neovide_{}", name.to_string());
            match nvim.get_var(&variable_name).await {
                Ok(value) => {
                    self.listeners.read().get(&name).unwrap()(value);
                }
                Err(error) => {
                    trace!("Initial value load failed for {}: {}", name, error);
                    let setting = self.readers.read().get(&name).unwrap()();
                    nvim.set_var(&variable_name, setting).await.ok();
                }
            }
        }
    }

    pub async fn setup_changed_listeners(&self, nvim: &Neovim<TxWrapper>) {
        let keys: Vec<String> = self.listeners.read().keys().cloned().collect();

        for name in keys {
            let vimscript = format!(
                concat!(
                    "exe \"",
                    "fun! NeovideNotify{0}Changed(d, k, z)\n",
                    "call rpcnotify(1, 'setting_changed', '{0}', g:neovide_{0})\n",
                    "endf\n",
                    "call dictwatcheradd(g:, 'neovide_{0}', 'NeovideNotify{0}Changed')\"",
                ),
                name
            );
            nvim.command(&vimscript)
                .await
                .unwrap_or_explained_panic(&format!(
                    "Could not setup setting notifier for {}",
                    name
                ));
        }
    }

    pub fn handle_changed_notification(&self, arguments: Vec<Value>) {
        let mut arguments = arguments.into_iter();
        let (name, value) = (arguments.next().unwrap(), arguments.next().unwrap());

        let name: Result<String, _> = name.try_into();
        let name = name.unwrap();

        self.listeners.read().get(&name).unwrap()(value);
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use nvim_rs::{Handler, Neovim};
    use tokio;

    use super::*;
    use crate::{
        bridge::{create, create_nvim_command},
        cmd_line::CmdLineSettings,
    };

    #[derive(Clone)]
    pub struct NeovimHandler();

    #[async_trait]
    impl Handler for NeovimHandler {
        type Writer = TxWrapper;

        async fn handle_notify(
            &self,
            _event_name: String,
            _arguments: Vec<Value>,
            _neovim: Neovim<TxWrapper>,
        ) {
        }
    }

    #[test]
    fn test_set_setting_handlers() {
        let settings = Settings::new();

        let property_name = "foo";

        fn noop_update(_v: Value) {}

        fn noop_read() -> Value {
            Value::Nil
        }

        settings.set_setting_handlers(property_name, noop_update, noop_read);
        let listeners = settings.listeners.read();
        let readers = settings.readers.read();
        let listener = listeners.get(property_name).unwrap();
        let reader = readers.get(property_name).unwrap();
        assert_eq!(&(noop_update as UpdateHandlerFunc), listener);
        assert_eq!(&(noop_read as ReaderFunc), reader);
    }

    #[test]
    fn test_set() {
        let settings = Settings::new();

        let v1: u32 = 1;
        let v2: f32 = 1.0;
        let vt1 = TypeId::of::<u32>();
        let vt2 = TypeId::of::<f32>();
        let v3: u32 = 2;

        settings.set(&v1);
        let values = settings.settings.read();
        let r1 = values.get(&vt1).unwrap().downcast_ref::<u32>().unwrap();
        assert_eq!(v1, *r1);

        settings.set(&v2);

        settings.set(&v3);

        let r2 = values.get(&vt1).unwrap().downcast_ref::<u32>().unwrap();
        let r3 = values.get(&vt2).unwrap().downcast_ref::<f32>().unwrap();

        assert_eq!(v3, *r2);
        assert_eq!(v2, *r3);
    }

    #[test]
    fn test_get() {
        let settings = Settings::new();

        let v1: u32 = 1;
        let v2: f32 = 1.0;
        let vt1 = TypeId::of::<u32>();
        let vt2 = TypeId::of::<f32>();

        let mut values = settings.settings.write();
        values.insert(vt1, Box::new(v1.clone()));
        values.insert(vt2, Box::new(v2.clone()));

        unsafe {
            settings.settings.force_unlock_write();
        }

        let r1 = settings.get::<u32>();
        let r2 = settings.get::<f32>();

        assert_eq!(v1, r1);
        assert_eq!(v2, r2);
    }

    #[tokio::test]
    async fn test_read_initial_values() {
        let settings = Settings::new();

        let v1: String = "foo".to_string();
        let v2: String = "bar".to_string();
        let v3: String = "baz".to_string();
        let v4: String = format!("neovide_{}", v1);
        let v5: String = format!("neovide_{}", v2);

        //create_nvim_command tries to read from CmdLineSettings.neovim_args
        //TODO: this sets a static variable. Can this have side effects on other tests?
        SETTINGS.set::<CmdLineSettings>(&CmdLineSettings::default());

        let (nvim, _) = create::new_child_cmd(&mut create_nvim_command(), NeovimHandler())
            .await
            .unwrap_or_explained_panic("Could not locate or start the neovim process");
        nvim.set_var(&v4, Value::from(v2.clone())).await.ok();

        fn noop_update(_v: Value) {}

        fn noop_read() -> Value {
            Value::from("baz".to_string())
        }

        let mut listeners = settings.listeners.write();
        listeners.insert(v1.clone(), noop_update);
        listeners.insert(v2.clone(), noop_update);

        unsafe {
            settings.listeners.force_unlock_write();
        }

        let mut readers = settings.readers.write();
        readers.insert(v1.clone(), noop_read);
        readers.insert(v2.clone(), noop_read);

        unsafe {
            settings.readers.force_unlock_write();
        }

        settings.read_initial_values(&nvim).await;

        let rt1 = nvim.get_var(&v4).await.unwrap();
        let rt2 = nvim.get_var(&v5).await.unwrap();

        let r1 = rt1.as_str().unwrap();
        let r2 = rt2.as_str().unwrap();

        assert_eq!(r1, v2);
        assert_eq!(r2, v3);
    }
}
