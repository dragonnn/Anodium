use smithay::{
    reexports::wayland_server::protocol::wl_output::{self, WlOutput},
    utils::{Logical, Point},
};

use crate::config::ConfigVM;

mod layer_map;
pub use layer_map::LayerSurface;

mod output;
pub use output::Output;

#[derive(Debug)]

pub struct OutputMap {
    outputs: Vec<Output>,

    config: ConfigVM,
}

impl OutputMap {
    pub fn new(config: ConfigVM) -> Self {
        Self {
            outputs: Vec::new(),

            config,
        }
    }

    pub fn rearrange(&mut self) {
        let configs = self.config.arrange_outputs(&self.outputs).unwrap();

        for config in configs {
            if let Some(output) = self.outputs.get_mut(config.id()) {
                output.set_location(config.location());

                let geometry = output.geometry();
                output.layer_map_mut().arange(geometry)
            }
        }
    }

    pub fn add(&mut self, output: Output) -> &Output {
        self.outputs.push(output);

        // We call arrange here albeit the output is only appended and
        // this would not affect windows, but arrange could re-organize
        // outputs from a configuration.
        self.rearrange();

        self.outputs.last().unwrap()
    }

    pub fn retain<F>(&mut self, f: F)
    where
        F: FnMut(&Output) -> bool,
    {
        self.outputs.retain(f);
        self.rearrange();
    }

    pub fn width(&self) -> i32 {
        // This is a simplification, we only arrange the outputs on the y axis side-by-side
        // so that the total width is simply the sum of all output widths.
        self.outputs
            .iter()
            .fold(0, |acc, output| acc + output.size().w)
    }

    pub fn height(&self, x: i32) -> Option<i32> {
        // This is a simplification, we only arrange the outputs on the y axis side-by-side
        self.outputs
            .iter()
            .find(|output| {
                let geometry = output.geometry();
                x >= geometry.loc.x && x < (geometry.loc.x + geometry.size.w)
            })
            .map(|output| output.size().h)
    }

    pub fn is_empty(&self) -> bool {
        self.outputs.is_empty()
    }

    pub fn find<F>(&self, f: F) -> Option<&Output>
    where
        F: FnMut(&&Output) -> bool,
    {
        self.outputs.iter().find(f)
    }

    #[allow(dead_code)]
    pub fn find_by_output(&self, output: &wl_output::WlOutput) -> Option<&Output> {
        self.find(|o| o.inner_output().owns(output))
    }

    #[allow(dead_code)]
    pub fn find_by_name<N>(&self, name: N) -> Option<&Output>
    where
        N: AsRef<str>,
    {
        self.find(|o| &o.name() == name.as_ref())
    }

    #[allow(dead_code)]
    pub fn find_by_position(&self, position: Point<i32, Logical>) -> Option<&Output> {
        self.find(|o| o.geometry().contains(position))
    }

    #[allow(dead_code)]
    pub fn find_by_index(&self, index: usize) -> Option<&Output> {
        self.outputs.get(index)
    }

    pub fn iter(&self) -> std::slice::Iter<Output> {
        self.outputs.iter()
    }
    pub fn iter_mut(&mut self) -> std::slice::IterMut<Output> {
        self.outputs.iter_mut()
    }

    pub fn refresh(&mut self) {
        for output in self.outputs.iter_mut() {
            output.layer_map_mut().refresh();
        }
    }
}

impl OutputMap {
    pub fn arrange_layers(&mut self) {
        for output in self.outputs.iter_mut() {
            let geometry = output.geometry();
            output.layer_map_mut().arange(geometry);
        }
    }

    pub fn insert_layer(&mut self, output: Option<WlOutput>, layer: LayerSurface) {
        let output = output.and_then(|output| {
            self.outputs
                .iter_mut()
                .find(|o| o.inner_output().owns(&output))
        });

        if let Some(output) = output {
            let geometry = output.geometry();
            let mut layer_map = output.layer_map_mut();

            layer_map.insert(layer);
            layer_map.arange(geometry);
        } else if let Some(output) = self.outputs.get_mut(0) {
            let geometry = output.geometry();
            let mut layer_map = output.layer_map_mut();

            layer_map.insert(layer);
            layer_map.arange(geometry);
        }
    }

    pub fn send_frames(&self, time: u32) {
        for output in self.outputs.iter() {
            output.layer_map().send_frames(time);
        }
    }
}