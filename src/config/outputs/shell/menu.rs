use std::cell::RefCell;
use std::rc::Rc;

use egui::Ui;
use rhai::plugin::*;
use rhai::Engine;
use rhai::FnPtr;

use super::widget::*;

#[derive(Debug)]
struct MenuInner {
    label: String,
}

#[derive(Debug, Clone)]
pub struct Menu(Rc<RefCell<MenuInner>>);

impl Menu {
    pub fn new(label: String) -> Self {
        Self(Rc::new(RefCell::new(MenuInner { label })))
    }
}

impl Widget for Menu {
    fn render(&self, ui: &mut Ui, _config_tx: &Sender<ConfigEvent>) {
        let inner = self.0.borrow();
        egui::menu::menu_button(ui, &inner.label, |ui| {
            if ui.button("Open").clicked() {
                // …
            }
        });
    }
}

#[export_module]
pub mod menu {
    #[rhai_fn(global)]
    pub fn label(menu: &mut Menu, label: String) {
        menu.0.borrow_mut().label = label;
    }

    #[rhai_fn(global)]
    pub fn convert(button: &mut Menu) -> Rc<dyn Widget> {
        Rc::new(button.clone())
    }
}

pub fn register(engine: &mut Engine) {
    let menu_module = exported_module!(menu);
    engine
        .register_static_module("menu", menu_module.into())
        .register_type::<Menu>();
}
