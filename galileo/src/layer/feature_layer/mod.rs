use crate::layer::feature_layer::feature::Feature;
use crate::layer::feature_layer::feature_store::{
    FeatureContainer, FeatureContainerMut, FeatureEntry, FeatureStore, FeatureUpdate,
};
use crate::layer::feature_layer::symbol::Symbol;
use crate::layer::Layer;
use crate::messenger::Messenger;
use crate::render::{Canvas, RenderOptions};
use crate::view::MapView;
use feature_render_store::FeatureRenderStore;
use galileo_types::cartesian::impls::point::{Point2d, Point3d};
use galileo_types::cartesian::rect::Rect;
use galileo_types::cartesian::traits::cartesian_point::{
    CartesianPoint2d, NewCartesianPoint2d, NewCartesianPoint3d,
};
use galileo_types::geo::crs::Crs;
use galileo_types::geo::impls::point::GeoPoint2d;
use galileo_types::geo::impls::projection::dimensions::AddDimensionProjection;
use galileo_types::geo::impls::projection::identity::IdentityProjection;
use galileo_types::geo::traits::point::NewGeoPoint;
use galileo_types::geo::traits::projection::{ChainProjection, InvertedProjection, Projection};
use galileo_types::geometry::{CartesianGeometry2d, Geom, Geometry};
use galileo_types::geometry_type::{CartesianSpace2d, CartesianSpace3d, GeoSpace2d};
use maybe_sync::{MaybeSend, MaybeSync};
use num_traits::AsPrimitive;
use std::any::Any;
use std::marker::PhantomData;
use std::ops::Deref;
use std::sync::{Mutex, RwLock};

pub mod feature;
pub mod feature_render_store;
pub mod feature_store;
pub mod symbol;

/// Feature layers render a set of [features](Feature) using [symbols](Symbol).
///
/// After the layer is created, the [internal features storage](FeatureStore) can be accessed through [FeatureLayer::features] and
/// [FeatureLayer::features_mut] methods. This storage provides methods to edit features or hide/show them without
/// deleting from the layer.
pub struct FeatureLayer<P, F, S, Space>
where
    F: Feature,
    F::Geom: Geometry<Point = P>,
{
    features: FeatureStore<F>,
    symbol: S,
    crs: Crs,
    lods: Vec<Lod>,
    messenger: RwLock<Option<Box<dyn Messenger>>>,
    options: FeatureLayerOptions,

    space: PhantomData<Space>,
}

/// Configuration of a [FeatureLayer].
#[derive(Debug, Copy, Clone)]
pub struct FeatureLayerOptions {
    /// If set to true, images drawn by the layer will be sorted by the depth value (relative to viewer) before being
    /// rendered.
    ///
    /// This option is useful for layers that render points as images, and when the map is rendered in 3D you want the
    /// images that are positioned behind other pins to be drawn behind. Without this option, the images are drawn in
    /// the order they are added to the feature list.
    ///
    /// Use this with caution though, as turning on this option affects performance drastically. You probably don't want
    /// it if the layer will have more then a 1000 images drawn. If you decide to use this option for larger layers
    /// anyway, don't forget to also increase [`buffer_size_limit`](FeatureLayerOptions::buffer_size_limit) as only
    /// features from the same buffer will be sorted.
    pub sort_by_depth: bool,

    /// Sets up a soft limit on the internal GPU buffers' size (in bytes) used to render this layer. Larger values
    /// slightly improve performance when rendering, bun drastically improve performance when updating just a
    /// few features from the set.
    pub buffer_size_limit: usize,

    /// If set to true, the layer will be rendered with anti-aliasing. It makes rendered lines look smoother but is a
    /// little less performant.
    pub use_antialiasing: bool,
}

impl Default for FeatureLayerOptions {
    fn default() -> Self {
        Self {
            sort_by_depth: false,
            buffer_size_limit: 10_000_000,
            use_antialiasing: true,
        }
    }
}

struct Lod {
    min_resolution: f64,
    contents: Mutex<FeatureRenderStore>,
}

impl Lod {
    fn new(id: usize, min_resolution: f64, buffer_size_limit: usize) -> Self {
        Self {
            min_resolution,
            contents: Mutex::new(FeatureRenderStore::new(
                id,
                min_resolution,
                buffer_size_limit,
            )),
        }
    }
}

impl<P, F, S, Space> FeatureLayer<P, F, S, Space>
where
    F: Feature,
    F::Geom: Geometry<Point = P>,
    S: Symbol<F>,
{
    pub fn new(features: Vec<F>, style: S, crs: Crs) -> Self {
        let options = FeatureLayerOptions::default();
        Self {
            features: FeatureStore::new(features.into_iter()),
            symbol: style,
            crs,
            messenger: RwLock::new(None),
            lods: vec![Lod::new(0, 1.0, options.buffer_size_limit)],
            options,
            space: Default::default(),
        }
    }

    pub fn with_lods(features: Vec<F>, style: S, crs: Crs, lods: &[f64]) -> Self {
        let options = FeatureLayerOptions::default();
        let mut lods: Vec<_> = lods
            .iter()
            .enumerate()
            .map(|(id, &min_resolution)| Lod::new(id, min_resolution, options.buffer_size_limit))
            .collect();
        lods.sort_by(|a, b| b.min_resolution.total_cmp(&a.min_resolution));

        Self {
            features: FeatureStore::new(features.into_iter()),
            symbol: style,
            crs,
            messenger: RwLock::new(None),
            lods,
            options,
            space: Default::default(),
        }
    }

    pub fn with_options(mut self, options: FeatureLayerOptions) -> Self {
        self.options = options;

        for lod in &mut self.lods {
            let lock = lod.contents.get_mut().expect("mutex is poisoned");
            lock.set_buffer_size_limit(options.buffer_size_limit);
        }

        self
    }
}

impl<P, F, S> FeatureLayer<P, F, S, GeoSpace2d>
where
    P: NewGeoPoint + 'static,
    F: Feature,
    F::Geom: Geometry<Point = P>,
{
    pub fn extent_projected(&self, crs: &Crs) -> Option<Rect> {
        let projection = crs.get_projection::<P, Point2d>()?;
        self.features
            .iter()
            .filter_map(|f| f.as_ref().geometry().project(&*projection))
            .map(|g| g.bounding_rectangle())
            .collect()
    }
}

impl<P, F, S> FeatureLayer<P, F, S, CartesianSpace2d>
where
    P: CartesianPoint2d,
    F: Feature,
    F::Geom: Geometry<Point = P>,
{
    /// Returns an iterator of features that are withing `tolerance` units from the `point`. Note that the `point` is
    /// expected to be set in the layer's CRS.
    ///
    /// At this moment this method just iterates over all features checking for each one if it is at the point. But
    /// in future it may be changed into using geo-index to make this more efficient. So this method should be preferred
    /// to manually checking every feature.
    pub fn get_features_at<'a>(
        &'a self,
        point: &'a impl CartesianPoint2d<Num = P::Num>,
        tolerance: P::Num,
    ) -> impl Iterator<Item = FeatureContainer<'a, F>> + 'a
    where
        F::Geom: CartesianGeometry2d<P>,
    {
        self.features
            .iter()
            .filter(move |f| f.as_ref().geometry().is_point_inside(point, tolerance))
    }

    /// Returns a mutable iterator of features that are withing `tolerance` units from the `point`. Note that the `point` is
    /// expected to be set in the layer's CRS.
    ///
    /// At this moment this method just iterates over all features checking for each one if it is at the point. But
    /// in future it may be changed into using geo-index to make this more efficient. So this method should be preferred
    /// to manually checking every feature.
    pub fn get_features_at_mut<'a>(
        &'a mut self,
        point: &'a impl CartesianPoint2d<Num = P::Num>,
        tolerance: P::Num,
    ) -> impl Iterator<Item = FeatureContainerMut<'a, F>> + 'a
    where
        F::Geom: CartesianGeometry2d<P>,
    {
        self.features
            .iter_mut()
            .filter(move |f| f.as_ref().geometry().is_point_inside(point, tolerance))
    }

    /// Returns a reference to the feature store.
    pub fn features(&self) -> &FeatureStore<F> {
        &self.features
    }

    /// Returns a mutable reference to the feature store.
    pub fn features_mut(&mut self) -> &mut FeatureStore<F> {
        &mut self.features
    }

    /// Returns the CRS of the layer.
    pub fn crs(&self) -> &Crs {
        &self.crs
    }
}

impl<P, F, S, Space> FeatureLayer<P, F, S, Space>
where
    F: Feature,
    F::Geom: Geometry<Point = P>,
    S: Symbol<F>,
{
    fn select_lod(&self, resolution: f64) -> &Mutex<FeatureRenderStore> {
        debug_assert!(!self.lods.is_empty());

        for lod in &self.lods {
            if lod.min_resolution < resolution {
                return &lod.contents;
            }
        }

        &self.lods[self.lods.len() - 1].contents
    }

    fn render_with_projection<Proj: Projection<InPoint = P, OutPoint = Point3d> + ?Sized>(
        &self,
        view: &MapView,
        canvas: &mut dyn Canvas,
        projection: impl Deref<Target = Proj>,
    ) {
        let updates = self.features.drain_updates();
        if !updates.is_empty() {
            self.update_feature_renders(canvas, projection, &updates);
        }

        let lod = self
            .select_lod(view.resolution())
            .lock()
            .expect("mutex is poisoned");

        canvas.draw_bundles(
            &lod.bundles(),
            RenderOptions {
                antialias: self.options.use_antialiasing,
            },
        );
    }

    fn update_feature_renders<Proj: Projection<InPoint = P, OutPoint = Point3d> + ?Sized>(
        &self,
        canvas: &dyn Canvas,
        projection: impl Deref<Target = Proj>,
        updates: &[FeatureUpdate],
    ) {
        for update in updates {
            if let FeatureUpdate::Delete { render_indices } = update {
                for (render_index, lod_index) in render_indices
                    .iter()
                    .enumerate()
                    .filter_map(|(lod_index, render_index)| render_index.map(|v| (v, lod_index)))
                {
                    self.lods[lod_index]
                        .contents
                        .lock()
                        .expect("mutex is poisoned")
                        .remove_render(render_index);
                }
            }
        }

        for lod in &self.lods {
            let mut lod = lod.contents.lock().expect("mutex is poisoned");

            for update in updates {
                lod.init_bundle(|| canvas.create_bundle());

                match update {
                    FeatureUpdate::Update { feature_index } => {
                        let Some(feature_entry) = self.features.get_entry(*feature_index) else {
                            log::warn!("Feature {feature_index} is not present in the store");
                            continue;
                        };

                        if let Some(render_index) = feature_entry.render_index(lod.id()) {
                            lod.remove_render(render_index);
                        }

                        self.render_feature(feature_entry, &*projection, &mut lod);
                    }
                    FeatureUpdate::UpdateStyle { feature_index } => {
                        let Some(feature_entry) = self.features.get_entry(*feature_index) else {
                            log::warn!("Feature {feature_index} is not present in the store");
                            continue;
                        };

                        if let Some(render_index) = feature_entry.render_index(lod.id()) {
                            self.update_feature(
                                feature_entry.feature(),
                                &*projection,
                                render_index,
                                &mut lod,
                            );
                        }
                    }
                    _ => {}
                }
            }

            lod.pack(canvas);
        }
    }

    fn render_feature<Proj: Projection<InPoint = P, OutPoint = Point3d> + ?Sized>(
        &self,
        feature_entry: &FeatureEntry<F>,
        projection: &Proj,
        lod: &mut FeatureRenderStore,
    ) {
        let feature = feature_entry.feature();
        let Some(projected): Option<Geom<Point3d>> = feature.geometry().project(projection) else {
            return;
        };

        let primitives = self
            .symbol
            .render(feature, &projected, lod.min_resolution());
        let index = lod.add_primitives(primitives);
        feature_entry.set_render_index(index, lod.id());
    }

    fn update_feature<Proj: Projection<InPoint = P, OutPoint = Point3d> + ?Sized>(
        &self,
        feature: &F,
        projection: &Proj,
        render_index: usize,
        lod: &mut FeatureRenderStore,
    ) {
        let Some(projected): Option<Geom<Point3d>> = feature.geometry().project(projection) else {
            return;
        };

        let primitives = self
            .symbol
            .render(feature, &projected, lod.min_resolution());
        lod.update_renders(render_index, primitives);
    }
}

impl<P, F, S> FeatureLayer<P, F, S, GeoSpace2d>
where
    P: NewGeoPoint + 'static,
    F: Feature + MaybeSend + MaybeSync + 'static,
    F::Geom: Geometry<Point = P>,
    S: Symbol<F> + MaybeSend + MaybeSync + 'static,
{
    fn get_projection(&self, crs: &Crs) -> impl Projection<InPoint = P, OutPoint = Point3d> {
        ChainProjection::new(
            crs.get_projection::<P, Point2d>().unwrap(),
            Box::new(AddDimensionProjection::new(0.0)),
        )
    }
}

impl<P, F, S> Layer for FeatureLayer<P, F, S, GeoSpace2d>
where
    P: NewGeoPoint + 'static,
    F: Feature + MaybeSend + MaybeSync + 'static,
    F::Geom: Geometry<Point = P>,
    S: Symbol<F> + MaybeSend + MaybeSync + 'static,
{
    fn render(&self, view: &MapView, canvas: &mut dyn Canvas) {
        let projection = self.get_projection(view.crs());
        self.render_with_projection(view, canvas, &projection);
    }

    fn prepare(&self, _view: &MapView) {
        // do nothing
    }

    fn set_messenger(&mut self, messenger: Box<dyn Messenger>) {
        *self.messenger.write().unwrap() = Some(messenger);
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

impl<P, F, S> FeatureLayer<P, F, S, CartesianSpace2d>
where
    P: NewCartesianPoint2d + Clone + 'static,
    F: Feature + MaybeSend + MaybeSync + 'static,
    F::Geom: Geometry<Point = P>,
    S: Symbol<F> + MaybeSend + MaybeSync + 'static,
{
    fn get_projection(&self, crs: &Crs) -> Box<dyn Projection<InPoint = P, OutPoint = Point3d>> {
        if crs == &self.crs {
            Box::new(AddDimensionProjection::new(0.0))
        } else {
            let self_proj = self.crs.get_projection::<GeoPoint2d, P>().unwrap();
            let view_proj: Box<dyn Projection<InPoint = _, OutPoint = Point2d>> =
                crs.get_projection().unwrap();
            Box::new(ChainProjection::new(
                Box::new(ChainProjection::new(
                    Box::new(InvertedProjection::new(self_proj)),
                    view_proj,
                )),
                Box::new(AddDimensionProjection::new(0.0)),
            ))
        }
    }
}

impl<P, F, S> Layer for FeatureLayer<P, F, S, CartesianSpace2d>
where
    P: NewCartesianPoint2d + Clone + 'static,
    F: Feature + MaybeSend + MaybeSync + 'static,
    F::Geom: Geometry<Point = P>,
    S: Symbol<F> + MaybeSend + MaybeSync + 'static,
{
    fn render(&self, view: &MapView, canvas: &mut dyn Canvas) {
        let projection = self.get_projection(view.crs());
        self.render_with_projection(view, canvas, projection);
    }

    fn prepare(&self, _view: &MapView) {
        // do nothing
    }

    fn set_messenger(&mut self, messenger: Box<dyn Messenger>) {
        *self.messenger.write().unwrap() = Some(messenger);
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

impl<P, F, S> FeatureLayer<P, F, S, CartesianSpace3d>
where
    P: NewCartesianPoint3d + 'static,
    P::Num: AsPrimitive<f32>,
    F: Feature + MaybeSend + MaybeSync + 'static,
    F::Geom: Geometry<Point = P>,
    S: Symbol<F> + MaybeSend + MaybeSync + 'static,
{
    fn get_projection(&self) -> IdentityProjection<P, Point3d, CartesianSpace3d> {
        IdentityProjection::new()
    }
}

impl<P, F, S> Layer for FeatureLayer<P, F, S, CartesianSpace3d>
where
    P: NewCartesianPoint3d + 'static,
    P::Num: AsPrimitive<f32>,
    F: Feature + MaybeSend + MaybeSync + 'static,
    F::Geom: Geometry<Point = P>,
    S: Symbol<F> + MaybeSend + MaybeSync + 'static,
{
    fn render(&self, view: &MapView, canvas: &mut dyn Canvas) {
        if view.crs() != &self.crs {
            // not supported at the moment for 3d coordiantes
            return;
        }

        let projection = self.get_projection();
        self.render_with_projection(view, canvas, &projection);
    }

    fn prepare(&self, _view: &MapView) {
        // do nothing
    }

    fn set_messenger(&mut self, messenger: Box<dyn Messenger>) {
        *self.messenger.write().unwrap() = Some(messenger);
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}
