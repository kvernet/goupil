use anyhow::Result;
use crate::numerics::Float;
use crate::transport::{
    density::DensityModel,
    geometry::{ExternalGeometry, ExternalTracer, GeometryDefinition, GeometryTracer,
               SimpleGeometry, stratified::MapData, StratifiedGeometry, StratifiedTracer,
               TopographyMap, TopographySurface},
    PhotonState,
};
use pyo3::prelude::*;
use pyo3::types::PyTuple;
use std::rc::Rc;
use super::ctrlc_catched;
use super::macros::value_error;
use super::materials::{MaterialLike, PyMaterialDefinition};
use super::numpy::{ArrayOrFloat, PyArray, PyArrayFlags, PyScalar};
use super::transport::CState;


// ===============================================================================================
// Python wrapper for a description of a geometry sector.
// ===============================================================================================

#[pyclass(name = "GeometrySector", module = "goupil")]
pub struct PyGeometrySector {
    #[pyo3(get)]
    material: PyObject,
    #[pyo3(get)]
    density: PyObject,
    #[pyo3(get)]
    description: Option<String>,
}

#[pymethods]
impl PyGeometrySector {
    #[new]
    fn new(
        py: Python,
        material: MaterialLike,
        density: PyObject,
        description: Option<&str>
    ) -> Result<Self> {
        let material: PyObject = material.pack(py)?;
        let _: DensityModel = density.extract(py)?; // type check.
        let description = description.map(|s| s.to_string());
        let result = Self { material, density, description };
        Ok(result)
    }

    fn __repr__(&self, py: Python) -> Result<String> {
        let material = self.material
            .as_ref(py)
            .repr()?
            .to_str()?;
        let density = self.density
            .as_ref(py)
            .repr()?
            .to_str()?;
        let result = match self.description.as_ref() {
            None => format!(
                "GeometrySector({}, {})",
                material,
                density,
            ),
            Some(description) => format!(
                "GeometrySector({}, {}, '{}')",
                material,
                density,
                description,
            ),
        };
        Ok(result)
    }
}


// ===============================================================================================
// Python wrapper for a simple geometry object.
// ===============================================================================================

#[pyclass(name = "SimpleGeometry", module = "goupil")]
pub struct PySimpleGeometry (pub SimpleGeometry);

#[pymethods]
impl PySimpleGeometry {
    #[new]
    fn new(
        material: MaterialLike,
        density: DensityModel,
    ) -> Result<Self> {
        let material = material.unpack()?;
        let geometry = SimpleGeometry::new(&material, density);
        Ok(Self(geometry))
    }

    #[getter]
    fn get_density(&self, py: Python) -> PyObject {
        self.0.sectors[0].density.into_py(py)
    }

    #[setter]
    fn set_density(&mut self, value: DensityModel) -> Result<()> {
        self.0.sectors[0].density = value;
        Ok(())
    }

    #[getter]
    fn get_material(&self) -> PyMaterialDefinition {
        PyMaterialDefinition(self.0.materials[0].clone())
    }
}


// ===============================================================================================
// Python wrapper for an external geometry object.
// ===============================================================================================

#[pyclass(name = "ExternalGeometry", module = "goupil")]
pub struct PyExternalGeometry {
    pub inner: ExternalGeometry,

    #[pyo3(get)]
    materials: PyObject,
    #[pyo3(get)]
    sectors: PyObject,
}

#[pymethods]
impl PyExternalGeometry {
    #[new]
    pub fn new(py: Python, path: &str) -> Result<Self> {
        let inner = unsafe { ExternalGeometry::new(path)? };
        let materials: &PyTuple = {
            let mut materials = Vec::<PyObject>::with_capacity(inner.materials.len());
            for material in inner.materials.iter() {
                let material = PyMaterialDefinition(material.clone());
                materials.push(material.into_py(py));
            }
            PyTuple::new(py, materials)
        };
        let sectors: PyObject = {
            let sectors: std::result::Result<Vec<_>, _> = inner
                .sectors
                .iter()
                .map(|sector| Py::new(py, PyGeometrySector {
                    material: (&materials[sector.material]).into_py(py),
                    density: sector.density.into_py(py),
                    description: sector.description
                        .as_ref()
                        .map(|description| description.to_string()),
                }))
                .collect();
            PyTuple::new(py, sectors?).into_py(py)
        };
        let materials: PyObject = materials.into_py(py);
        let result = Self { inner, materials, sectors };
        Ok(result)
    }

    fn locate(&self, states: &PyArray<CState>) -> Result<PyObject> {
        locate::<ExternalGeometry, ExternalTracer>(&self.inner, states)
    }

    fn trace(
        &self,
        states: &PyArray<CState>,
        lengths: Option<ArrayOrFloat>,
        density: Option<bool>,
    ) -> Result<PyObject> {
        trace::<ExternalGeometry, ExternalTracer>(&self.inner, states, lengths, density)
    }

    fn update_material(
        &mut self,
        py: Python,
        index: usize,
        material: MaterialLike,
    ) -> Result<()> {
        // Update inner state.
        let material = material.unpack()?;
        self.inner.update_material(index, &material)?;

        // Update external state.
        let materials: &PyTuple = self.materials.extract(py)?;
        let mut this: PyRefMut<PyMaterialDefinition> = materials[index].extract()?;
        this.0 = material.into_owned();

        Ok(())
    }

    fn update_sector(
        &mut self,
        py: Python,
        index: usize,
        material: Option<usize>,
        density: Option<DensityModel>,
    ) -> Result<()> {
        // Update inner state.
        self.inner.update_sector(index, material, density.as_ref())?;

        // Update external state.
        let sectors: &PyTuple = self.sectors.extract(py)?;
        let mut this: PyRefMut<PyGeometrySector> = sectors[index].extract()?;
        if let Some(material) = material {
            let materials: &PyTuple = self.materials.extract(py)?;
            this.material = materials[material].into_py(py);
        }
        if let Some(density) = density.as_ref() {
            this.density = density.into_py(py);
        }

        Ok(())
    }
}


// ===============================================================================================
// Python wrapper for a topography map object.
// ===============================================================================================

#[pyclass(name = "TopographyMap", module = "goupil")]
pub struct PyTopographyMap {
    inner: Rc<TopographyMap>,

    #[pyo3(get)]
    x: PyObject,
    #[pyo3(get)]
    y: PyObject,
    #[pyo3(get)]
    z: PyObject,
}

unsafe impl Send for PyTopographyMap {}

#[pymethods]
impl PyTopographyMap {
    #[new]
    fn new(
        py: Python,
        xrange: [Float; 2],
        yrange: [Float; 2],
        z: Option<ArrayOrFloat>,
    ) -> Result<Py<Self>> {
        let shape = match z.as_ref() {
            None => None,
            Some(z) => match z {
                ArrayOrFloat::Array(z) => {
                    let shape = z.shape();
                    if shape.len() != 2 {
                        value_error!(
                            "bad shape for z-array (expected a 2D array, found a {}D array)",
                            shape.len(),
                        )
                    }
                    Some([shape[0], shape[1]])
                },
                ArrayOrFloat::Float(_) => None,
            },
        };

        let range = |min, max, n| -> Result<PyObject> {
            let array = PyArray::<Float>::empty(py, &[n])?;
            array.set(0, min)?;
            let delta = (max - min) / ((n - 1) as Float);
            for i in 1..(n-1) {
                let v = delta * (i as Float) + min;
                array.set(i, v)?;
            }
            array.set(n - 1, max)?;
            array.readonly();
            Ok(array.into_py(py))
        };

        // Build map.
        let mut map = TopographyMap::new(
            &xrange, &yrange, shape.as_ref()
        );
        if let Some(z) = z.as_ref() {
            match &mut map.z {
                MapData::Interpolator(interpolator) => {
                    let shape = shape.unwrap();
                    for i in 0..shape[0] {
                        for j in 0..shape[1] {
                            let k = i * shape[1] + j;
                            let zij = match z {
                                ArrayOrFloat::Array(z) => z.get(k)?,
                                ArrayOrFloat::Float(z) => *z,
                            };
                            interpolator[(i, j)]  = zij;
                            if zij < map.zmin { map.zmin = zij };
                            if zij > map.zmax { map.zmax = zij };
                        }
                    }
                },
                MapData::Scalar(value) => {
                    let z = match z {
                        ArrayOrFloat::Float(z) => *z,
                        _ => unreachable!(),
                    };
                    *value = z;
                    map.zmin = z;
                    map.zmax = z;
                },
            }
        }

        // Build typed Py-object.
        let inner = Rc::new(map);
        let (nx, ny) = match shape {
            None => (2, 2),
            Some(shape) => (shape[1], shape[0]),
        };
        let x = range(xrange[0], xrange[1], nx)?;
        let y = range(yrange[0], yrange[1], ny)?;
        let z = py.None();
        let result = Py::new(py, Self { inner, x, y, z })?;

        // Create view of z-data, linked to Py-object.
        let z: &PyAny = match &result.borrow(py).inner.z {
            MapData::Interpolator(interpolator) => PyArray::from_data(
                py,
                interpolator.as_ref(),
                result.as_ref(py),
                PyArrayFlags::ReadOnly,
                Some(&shape.unwrap()),
            )?,
            MapData::Scalar(z) => PyScalar::<Float>::new(py, *z)?.as_ref(),
        };

        let z: PyObject = z.into();
        result.borrow_mut(py).z = z;
        Ok(result)
    }

    fn __add__(lhs: PyRef<Self>, rhs: Float) -> PyTopographySurface {
        let py = lhs.py();
        let map: PyObject = lhs.into_py(py);
        let maps: Py<PyTuple> = (map,).into_py(py);
        PyTopographySurface::new(maps.as_ref(py), Some(rhs)).unwrap()
    }

    fn __radd__(rhs: PyRef<Self>, lhs: Float) -> PyTopographySurface {
        Self::__add__(rhs, lhs)
    }

    fn __sub__(lhs: PyRef<Self>, rhs: Float) -> PyTopographySurface {
        Self::__add__(lhs, -rhs)
    }

    fn __call__(
        &self,
        py: Python,
        x: ArrayOrFloat,
        y: ArrayOrFloat,
        grid: Option<bool>
    ) -> Result<PyObject> {
        self.compute_z_vec(py, x, y, grid)
    }

    #[getter]
    fn get_box(&self) -> ((Float, Float), (Float, Float)) {
        let b = self.inner.get_box();
        ((b.xmin, b.xmax), (b.ymin, b.ymax))
    }
}

impl ComputeZ for PyTopographyMap {
    fn compute_z(&self, x: Float, y: Float) -> ComputeZResult {
        let z = self.inner.z(x, y);
        ComputeZResult::One(z)
    }

    fn compute_z_size(&self) -> usize { 0 }
}


// ===============================================================================================
// Python wrapper for a topography surface object.
// ===============================================================================================

#[pyclass(name = "TopographySurface", module = "goupil")]
pub struct PyTopographySurface {
    inner: TopographySurface,
    maps: Py<PyTuple>,
}

unsafe impl Send for PyTopographySurface {}

#[pymethods]
impl PyTopographySurface {
    #[new]
    #[pyo3(signature = (*args, offset=None))]
    fn new(args: &PyTuple, offset: Option<Float>) -> Result<Self> {
        let py = args.py();
        let maps: PyResult<Vec<_>> = args
            .iter()
            .map(|arg| -> PyResult<Rc<TopographyMap>> {
                let map: PyRef<PyTopographyMap> = arg.extract()?;
                Ok(Rc::clone(&map.inner))
            })
            .collect();
        let maps = maps?;
        let maps: Vec<_> = maps
            .iter()
            .collect();
        let inner = {
            let mut inner = TopographySurface::new(&maps)?;
            if let Some(offset) = offset {
                inner.offset = offset;
            }
            inner
        };
        let maps: Py<PyTuple> = args.into_py(py);
        let result = Self { inner, maps };
        Ok(result)
    }

    fn __add__(lhs: PyRef<Self>, rhs: Float) -> Self {
        let py = lhs.py();
        let maps = Py::clone_ref(&lhs.maps, py);
        let mut inner = lhs.inner.clone();
        inner.offset += rhs;
        Self { inner, maps }
    }

    fn __radd__(rhs: PyRef<Self>, lhs: Float) -> Self {
        Self::__add__(rhs, lhs)
    }

    fn __sub__(lhs: PyRef<Self>, rhs: Float) -> Self {
        Self::__add__(lhs, -rhs)
    }

    fn __call__(
        &self,
        py: Python,
        x: ArrayOrFloat,
        y: ArrayOrFloat,
        grid: Option<bool>
    ) -> Result<PyObject> {
        self.compute_z_vec(py, x, y, grid)
    }
}

impl ComputeZ for PyTopographySurface {
    fn compute_z(&self, x: Float, y: Float) -> ComputeZResult {
        let z = self.inner.z(x, y);
        ComputeZResult::One(z)
    }

    fn compute_z_size(&self) -> usize { 0 }
}


// ===============================================================================================
// Python wrapper for a stratified geometry object.
// ===============================================================================================

#[pyclass(name = "StratifiedGeometry", module = "goupil")]
pub struct PyStratifiedGeometry {
    pub(crate) inner: StratifiedGeometry,

    #[pyo3(get)]
    materials: PyObject,
    #[pyo3(get)]
    sectors: PyObject,
}

unsafe impl Send for PyStratifiedGeometry {}

#[pymethods]
impl PyStratifiedGeometry {
    #[new]
    #[pyo3(signature = (*args))]
    pub fn new(args: &PyTuple) -> Result<Self> {
        let py = args.py();

        let n = args.len();
        if n == 0 {
            value_error!(
                "bad number of argument(s) (expected one or more argument(s), found zero)"
            )
        }

        // Initialise inner geometry object.
        let (mut last, sector, bottom) = {
            let result: std::result::Result<PyRef<PyGeometrySector>, _> = args[n - 1].extract();
            match result {
                std::result::Result::Err(_) => {
                    let bottom: MapOrSurface = args[n - 1].extract()?;
                    let bottom: TopographySurface = bottom.into();
                    let last = n - 2;
                    let sector: PyRef<PyGeometrySector> = args[last].extract()?;
                    (last, sector, Some(bottom))
                },
                std::result::Result::Ok(result) => {
                    (n - 1, result, None)
                },
            }
        };

        let material: PyRef<PyMaterialDefinition> = sector.material.extract(py)?;
        let density: DensityModel = sector.density.extract(py)?;
        let description = sector.description.as_ref().map(|s| s.as_str());
        let mut inner = StratifiedGeometry::new(&material.0, density, description);

        if let Some(bottom) = bottom {
            inner.set_bottom(&bottom);
        }

        // Loop over additional layers.
        while last > 1 {
            // Extract interface.
            let interface: MapOrSurface = args[last - 1].extract()?;
            let interface: TopographySurface = interface.into();

            // Extract sector.
            let sector: PyRef<PyGeometrySector> = args[last - 2].extract()?;
            let material: PyRef<PyMaterialDefinition> = sector.material.extract(py)?;
            let density: DensityModel = sector.density.extract(py)?;
            let description = sector.description.as_ref().map(|s| s.as_str());

            // Update the geometry.
            inner.push_layer(&interface, &material.0, density, description)?;
            last -= 2;
        }

        if last == 1 {
            let top: MapOrSurface = args[0].extract()?;
            let top: TopographySurface = top.into();
            inner.set_top(&top);
        }

        let inner = inner; // Lock mutability at this point.

        // Export materials and sectors.
        let materials: &PyTuple = {
            let mut materials = Vec::<PyObject>::with_capacity(inner.materials.len());
            for material in inner.materials.iter() {
                let material = PyMaterialDefinition(material.clone());
                materials.push(material.into_py(py));
            }
            PyTuple::new(py, materials)
        };
        let sectors: PyObject = {
            let sectors: std::result::Result<Vec<_>, _> = inner
                .sectors
                .iter()
                .map(|sector| Py::new(py, PyGeometrySector {
                    material: (&materials[sector.material]).into_py(py),
                    density: sector.density.into_py(py),
                    description: sector.description
                        .as_ref()
                        .map(|description| description.to_string()),
                }))
                .collect();
            PyTuple::new(py, sectors?).into_py(py)
        };
        let materials: PyObject = materials.into_py(py);

        // Wrap geometry and return.
        Ok(Self { inner, materials, sectors })
    }

    fn locate(&self, states: &PyArray<CState>) -> Result<PyObject> {
        locate::<StratifiedGeometry, StratifiedTracer>(&self.inner, states)
    }

    fn trace(
        &self,
        states: &PyArray<CState>,
        lengths: Option<ArrayOrFloat>,
        density: Option<bool>,
    ) -> Result<PyObject> {
        trace::<StratifiedGeometry, StratifiedTracer>(&self.inner, states, lengths, density)
    }

    fn z(
        &self,
        py: Python,
        x: ArrayOrFloat,
        y: ArrayOrFloat,
        grid: Option<bool>
    ) -> Result<PyObject> {
        self.compute_z_vec(py, x, y, grid)
    }
}

impl ComputeZ for PyStratifiedGeometry {
    fn compute_z(&self, x: Float, y: Float) -> ComputeZResult {
        let z = self.inner.z(x, y);
        ComputeZResult::Many(z)
    }

    fn compute_z_size(&self) -> usize {
        self.inner.sectors().len() + 1
    }
}

#[derive(FromPyObject)]
enum MapOrSurface<'py> {
    Map(PyRef<'py, PyTopographyMap>),
    Surface(PyRef<'py, PyTopographySurface>),
}

impl<'py> From<MapOrSurface<'py>> for TopographySurface {
    fn from(value: MapOrSurface) -> Self {
        match value {
            MapOrSurface::Map(value) => (&value.inner).into(),
            MapOrSurface::Surface(value) => value.inner.clone(),
        }
    }
}


// ===============================================================================================
// Generic geometry operations.
// ===============================================================================================

fn locate<'a, D: GeometryDefinition, T: GeometryTracer<'a, D>>(
    definition: &'a D,
    states: &PyArray<CState>
) -> Result<PyObject> {
    let py = states.py();
    let sectors = PyArray::<usize>::empty(py, &states.shape())?;
    let mut tracer = T::new(definition)?;
    let m = definition.sectors().len();
    let n = states.size();
    for i in 0..n {
        let state: PhotonState = states.get(i)?.into();
        tracer.reset(state.position, state.direction)?;
        let sector = tracer.sector().unwrap_or(m);
        sectors.set(i, sector)?;

        if i % 1000 == 0 { // Check for a Ctrl+C interrupt, catched by Python.
            ctrlc_catched()?;
        }
    }
    let sectors: &PyAny = sectors;
    Ok(sectors.into_py(py))
}

fn trace<'a, D: GeometryDefinition, T: GeometryTracer<'a, D>>(
    definition: &'a D,
    states: &PyArray<CState>,
    lengths: Option<ArrayOrFloat>,
    density: Option<bool>,
) -> Result<PyObject> {
    let n = states.size();
    if let Some(lengths) = lengths.as_ref() {
        if let ArrayOrFloat::Array(lengths) = &lengths {
            if lengths.size() != states.size() {
                value_error!(
                    "bad lengths (expected a float or a size {} array, found a size {} array)",
                    states.size(),
                    lengths.size(),
                )
            }
        }
    }

    let mut shape = states.shape();
    let m = definition.sectors().len();
    shape.push(m);
    let py = states.py();
    let result = PyArray::<Float>::empty(py, &shape)?;

    let density = density.unwrap_or(false);
    let mut tracer = T::new(definition)?;
    let mut k: usize = 0;
    for i in 0..n {
        let state: PhotonState = states.get(i)?.into();
        let mut grammages: Vec<Float> = vec![0.0; m];
        tracer.reset(state.position, state.direction)?;
        let mut length = match lengths.as_ref() {
            None => Float::INFINITY,
            Some(lengths) => match &lengths {
                ArrayOrFloat::Array(lengths) => lengths.get(i)?,
                ArrayOrFloat::Float(lengths) => *lengths,
            },
        };
        loop {
            match tracer.sector() {
                None => break,
                Some(sector) => {
                    let step_length = tracer.trace(length)?;
                    if density {
                        let model = definition.sectors()[sector].density;
                        let position = tracer.position();
                        grammages[sector] += model.column_depth(
                            position, state.direction, step_length
                        );
                    } else {
                        grammages[sector] += step_length;
                    }
                    if lengths.is_some() {
                        length -= step_length;
                        if length <= 0.0 { break }
                    }
                    tracer.update(step_length, state.direction)?;
                },
            }
            k += 1;
            if k == 1000 { // Check for a Ctrl+C interrupt, catched by Python.
                ctrlc_catched()?;
                k = 0;
            }
        }
        let j0 = i * m;
        for j in 0..m {
            result.set(j0 + j, grammages[j])?;
        }
    }
    let result: &PyAny = result;
    Ok(result.into_py(py))
}


// ===============================================================================================
// Generic z-computation.
// ===============================================================================================

trait ComputeZ {
    fn compute_z(&self, x: Float, y: Float) -> ComputeZResult;

    fn compute_z_size(&self) -> usize;

    fn compute_z_vec(
        &self,
        py: Python,
        x: ArrayOrFloat,
        y: ArrayOrFloat,
        grid: Option<bool>
    ) -> Result<PyObject> {
        let grid = grid.unwrap_or(false);
        let result: &PyAny = if grid {
            let nx = x.size();
            let ny = y.size();
            if (nx == 0) || (ny == 0) {
                value_error!(
                    "bad size (expected {{ny, nx}} > 0, found {{{}, {}}})",
                    ny,
                    nx,
                );
            }
            let nz = self.compute_z_size();
            let shape = match nz {
                0 => vec![ny, nx],
                _ => vec![ny, nx, nz],
            };
            let result = PyArray::<Float>::empty(py, &shape)?;
            for i in 0..ny {
                let yi = y.get(i)?;
                for j in 0..nx {
                    let xj = x.get(j)?;
                    let zij = self.compute_z(xj, yi);
                    match zij {
                        ComputeZResult::Many(zij) => {
                            for k in 0..nz {
                                let zijk = zij[k].unwrap_or(Float::NAN);
                                result.set(nz * (i * nx + j) + k, zijk)?;
                            }
                        },
                        ComputeZResult::One(zij) => {
                            let zij = zij.unwrap_or(Float::NAN);
                            result.set(i * nx + j, zij)?;
                        }
                    }
                }
            }
            let result: &PyAny = result;
            result
        } else {
            if x.is_float() && y.is_float() {
                let x = match x {
                    ArrayOrFloat::Float(x) => x,
                    _ => unreachable!(),
                };
                let y = match y {
                    ArrayOrFloat::Float(y) => y,
                    _ => unreachable!(),
                };
                let z = self.compute_z(x, y);
                match z {
                    ComputeZResult::Many(z) => {
                        let nz = self.compute_z_size();
                        let result = PyArray::<Float>::empty(py, &[nz])?;
                        for i in 0..nz {
                            let zi = z[i].unwrap_or(Float::NAN);
                            result.set(i, zi)?;
                        }
                        let result: &PyAny = result;
                        result
                    },
                    ComputeZResult::One(z) => {
                        let z = z.unwrap_or(Float::NAN);
                        let result = PyScalar::<Float>::new(py, z)?;
                        let result: &PyAny = result;
                        result
                    },
                }
            } else {
                let (n, mut shape) = match &x {
                    ArrayOrFloat::Array(x) => {
                        if let ArrayOrFloat::Array(y) = &y {
                            if x.size() != y.size() {
                                value_error!(
                                    "bad size (x and y arrays must have the same size)"
                                )
                            }
                        }
                        (x.size(), x.shape().to_vec())
                    },
                    ArrayOrFloat::Float(_) => match &y {
                        ArrayOrFloat::Array(y) => (y.size(), y.shape().to_vec()),
                        _ => unreachable!(),
                    }
                };

                let nz = self.compute_z_size();
                if nz > 0 {
                    shape.push(nz)
                }

                let result = PyArray::<Float>::empty(py, &shape)?;
                for i in 0..n {
                    let xi = x.get(i)?;
                    let yi = y.get(i)?;
                    let zi = self.compute_z(xi, yi);
                    match zi {
                        ComputeZResult::Many(zi) => {
                            for j in 0..nz {
                                let zij = zi[j].unwrap_or(Float::NAN);
                                result.set(i * nz  + j, zij)?
                            }
                        },
                        ComputeZResult::One(zi) => {
                            let zi = zi.unwrap_or(Float::NAN);
                            result.set(i, zi)?
                        }
                    }
                }

                let result: &PyAny = result;
                result
            }
        };
        Ok(result.into_py(py))
    }
}

enum ComputeZResult {
    Many(Vec<Option<Float>>),
    One(Option<Float>),
}


// ===============================================================================================
// Unresolved geometry definition.
// ===============================================================================================

#[derive(Clone, FromPyObject)]
pub enum PyGeometryDefinition {
    External(Py<PyExternalGeometry>),
    Simple(Py<PySimpleGeometry>),
    Stratified(Py<PyStratifiedGeometry>),
}

impl IntoPy<PyObject> for PyGeometryDefinition {
    fn into_py(self, py: Python) -> PyObject {
        match self {
            Self::External(external) => external.into_py(py),
            Self::Simple(simple) => simple.into_py(py),
            Self::Stratified(stratified) => stratified.into_py(py),
        }
    }
}
