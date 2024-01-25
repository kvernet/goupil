use anyhow::Result;
use crate::numerics::Float;
use crate::transport::{
    density::DensityModel,
    geometry::{ExternalGeometry, ExternalTracer, GeometryDefinition, GeometryTracer,
               SimpleGeometry, StratifiedGeometry, TopographyData, TopographyMap},
    PhotonState,
};
use pyo3::prelude::*;
use pyo3::types::PyTuple;
use std::rc::Rc;
use super::ctrlc_catched;
use super::macros::value_error;
use super::materials::PyMaterialDefinition;
use super::numpy::{ArrayOrFloat, PyArray, PyArrayFlags};
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
        material: PyRef<PyMaterialDefinition>, // XXX Allow for a string?
        density: PyObject,
        description: Option<&str>
    ) -> Result<Self> {
        let py = material.py();
        let material: PyObject = material.into_py(py);
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
        material: PyRef<PyMaterialDefinition>,
        density: DensityModel,
    ) -> Result<Self> {
        let geometry = SimpleGeometry::new(&material.0, density);
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
        let py = states.py();
        let sectors = PyArray::<usize>::empty(py, &states.shape())?;
        let mut tracer = ExternalTracer::new(&self.inner)?;
        let m = self.inner.sectors().len();
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

    fn trace(
        &self,
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
        let m = self.inner.sectors().len();
        shape.push(m);
        let py = states.py();
        let result = PyArray::<Float>::empty(py, &shape)?;

        let density = density.unwrap_or(false);
        let mut tracer = ExternalTracer::new(&self.inner)?;
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
                            let model = &self.inner.sectors[sector].density;
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

    fn update_material(
        &mut self,
        index: usize,
        material: PyRef<PyMaterialDefinition>
    ) -> Result<()> {
        // Update inner state.
        self.inner.update_material(index, &material.0)?;

        // Update external state.
        let py = material.py();
        let materials: &PyTuple = self.materials.extract(py)?;
        let mut this: PyRefMut<PyMaterialDefinition> = materials[index].extract()?;
        this.0 = material.0.clone();

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
        z: Option<&PyArray<Float>>,
        shape: Option<[usize; 2]>,
    ) -> Result<Py<Self>> {
        let shape = match shape {
            None => match z {
                None => value_error!(
                    "cannot infer map's shape (expected a length-2 sequence, found 'None')"
                ),
                Some(z) => {
                    let shape = z.shape();
                    if shape.len() != 2 {
                        value_error!(
                            "bad shape for z-array (expected a 2D array, found a {}D array)",
                            shape.len(),
                        )
                    }
                    [shape[0], shape[1]]
                },
            },
            Some(shape) => {
                if let Some(z) = z {
                    let size = shape[0] * shape[1];
                    if z.size() != size {
                        value_error!(
                            "bad size for z-array (expected {}, found {})",
                            size,
                            z.size()
                        )
                    }
                }
                shape
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
            xrange[0], xrange[1], shape[1], yrange[0], yrange[1], shape[0]
        );
        if let Some(z) = z {
            for i in 0..shape[0] {
                for j in 0..shape[1] {
                    let k = i * shape[1] + j;
                    map.z[(i, j)] = z.get(k)?;
                }
            }
        }

        // Build typed Py-object.
        let inner = Rc::new(map);
        let x = range(xrange[0], xrange[1], shape[1])?;
        let y = range(yrange[0], yrange[1], shape[0])?;
        let z = py.None();
        let result = Py::new(py, Self { inner, x, y, z })?;

        // Create view of z-data, linked to Py-object.
        let z: &PyAny = PyArray::from_data(
            py,
            result.borrow(py).inner.z.as_ref(),
            result.as_ref(py),
            PyArrayFlags::ReadWrite,
            Some(&shape),
        )?;
        let z: PyObject = z.into();
        result.borrow_mut(py).z = z;

        Ok(result)
    }

    fn __add__(lhs: PyRef<Self>, rhs: Float) -> PyTopographyOffset {
        let py = lhs.py();
        let map: PyObject = lhs.into_py(py);
        PyTopographyOffset { map, offset: rhs }
    }

    fn __radd__(rhs: PyRef<Self>, lhs: Float) -> PyTopographyOffset {
        Self::__add__(rhs, lhs)
    }

    fn __sub__(lhs: PyRef<Self>, rhs: Float) -> PyTopographyOffset {
        Self::__add__(lhs, -rhs)
    }

    fn __call__(&self, x: Float, y: Float) -> Option<Float> { // XXX vectorise and fill
        self.inner.z(x, y)
    }
}


// ===============================================================================================
// Python wrapper for a topography map offset.
// ===============================================================================================

#[pyclass(name = "TopographyOffset", module = "goupil")]
pub struct PyTopographyOffset {
    #[pyo3(get)]
    map: PyObject,
    #[pyo3(get)]
    offset: Float,
}

#[pymethods]
impl PyTopographyOffset {
    #[new]
    fn new(lhs: MapOrOffset, rhs: Float) -> Result<Self> {
        let result = match lhs {
            MapOrOffset::Map(map) => {
                let py = map.py();
                let map: PyObject = map.into_py(py);
                Self { map, offset: rhs }
            },
            MapOrOffset::Offset(offset) => {
                let py = offset.py();
                let map = Py::clone_ref(&offset.map, py);
                Self { map, offset: offset.offset + rhs }
            },
        };
        Ok(result)
    }

    fn __add__(lhs: PyRef<Self>, rhs: Float) -> PyTopographyOffset {
        let py = lhs.py();
        let map = Py::clone_ref(&lhs.map, py);
        PyTopographyOffset { map, offset: rhs + lhs.offset }
    }

    fn __radd__(rhs: PyRef<Self>, lhs: Float) -> PyTopographyOffset {
        Self::__add__(rhs, lhs)
    }

    fn __sub__(lhs: PyRef<Self>, rhs: Float) -> PyTopographyOffset {
        Self::__add__(lhs, -rhs)
    }

    // XXX Add call method?
}

#[derive(FromPyObject)]
enum MapOrOffset<'py> {
    Map(PyRef<'py, PyTopographyMap>),
    Offset(PyRef<'py, PyTopographyOffset>),
}


// ===============================================================================================
// Python wrapper for a stratified geometry object.
// ===============================================================================================

#[pyclass(name = "StratifiedGeometry", module = "goupil")]
pub struct PyStratifiedGeometry {
    inner: StratifiedGeometry,

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
                    let bottom: PyTopographyInterface = args[n - 1].extract()?;
                    let bottom: Vec<TopographyData> = bottom.into();
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
            let interface: PyTopographyInterface = args[last - 1].extract()?;
            let interface: Vec<TopographyData> = interface.into();

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
            let top: PyTopographyInterface = args[0].extract()?;
            let top: Vec<TopographyData> = top.into();
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
}

#[derive(FromPyObject)]
enum PyTopographyData<'py> {
    Constant(Float),
    Map(PyRef<'py, PyTopographyMap>),
    Offset(PyRef<'py, PyTopographyOffset>),
}

impl<'py> From<PyTopographyData<'py>> for TopographyData {
    fn from(value: PyTopographyData) -> Self {
        match value {
            PyTopographyData::Constant(value) => TopographyData::Constant(value),
            PyTopographyData::Map(value) => TopographyData::Map(Rc::clone(&value.inner)),
            PyTopographyData::Offset(value) => {
                let py = value.py();
                let map: PyRef<PyTopographyMap> = value.map.extract(py).unwrap();
                TopographyData::Offset(Rc::clone(&map.inner), value.offset)
            },
        }
    }
}

#[derive(FromPyObject)]
enum PyTopographyInterface<'py> { // XXX Export this type?
    Scalar(PyTopographyData<'py>),
    Sequence(Vec<PyTopographyData<'py>>),
}

impl<'py> From<PyTopographyInterface<'py>> for Vec<TopographyData> {
    fn from(value: PyTopographyInterface) -> Self {
        match value {
            PyTopographyInterface::Scalar(value) => {
                let value: TopographyData = value.into();
                vec![value]
            },
            PyTopographyInterface::Sequence(values) => {
                let mut result = Vec::<TopographyData>::with_capacity(values.len());
                for value in values {
                    let value: TopographyData = value.into();
                    result.push(value);
                }
                result
            }
        }
    }
}


// ===============================================================================================
// Unresolved geometry definition.
// ===============================================================================================

#[derive(Clone, FromPyObject)]
pub enum PyGeometryDefinition {
    External(Py<PyExternalGeometry>),
    Simple(Py<PySimpleGeometry>),
}

impl IntoPy<PyObject> for PyGeometryDefinition {
    fn into_py(self, py: Python) -> PyObject {
        match self {
            Self::External(external) => external.into_py(py),
            Self::Simple(simple) => simple.into_py(py),
        }
    }
}
