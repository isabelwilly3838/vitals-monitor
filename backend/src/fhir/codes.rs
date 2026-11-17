//! Terminology and clinical reference values (FHIR R4B Vital Signs profile).

pub const LOINC: &str = "http://loinc.org";
pub const UCUM: &str = "http://unitsofmeasure.org";
pub const OBS_CATEGORY: &str = "http://terminology.hl7.org/CodeSystem/observation-category";
pub const INTERPRETATION: &str =
    "http://terminology.hl7.org/CodeSystem/v3-ObservationInterpretation";

pub const PROFILE_VITALSIGNS: &str = "http://hl7.org/fhir/StructureDefinition/vitalsigns";
pub const PROFILE_BODYTEMP: &str = "http://hl7.org/fhir/StructureDefinition/bodytemp";
pub const PROFILE_BP: &str = "http://hl7.org/fhir/StructureDefinition/bp";

pub const CODE_BODY_TEMP: &str = "8310-5";
pub const CODE_BP_PANEL: &str = "85354-9";
pub const CODE_SYSTOLIC: &str = "8480-6";
pub const CODE_DIASTOLIC: &str = "8462-4";

pub fn display(code: &str) -> &'static str {
    match code {
        CODE_BODY_TEMP => "Body temperature",
        CODE_BP_PANEL => "Blood pressure panel with all children optional",
        CODE_SYSTOLIC => "Systolic blood pressure",
        CODE_DIASTOLIC => "Diastolic blood pressure",
        _ => "",
    }
}

pub const UNIT_CEL: &str = "Cel";
pub const UNIT_MMHG: &str = "mm[Hg]";

/// Observation.status (required binding).
pub const OBS_STATUS: [&str; 8] = [
    "registered",
    "preliminary",
    "final",
    "amended",
    "corrected",
    "cancelled",
    "entered-in-error",
    "unknown",
];

/// Physiologically *possible* ranges: outside these the reading is a sensor fault -> reject.
pub const TEMP_RANGE: (f64, f64) = (25.0, 45.0);
pub const SYS_RANGE: (f64, f64) = (40.0, 300.0);
pub const DIA_RANGE: (f64, f64) = (20.0, 200.0);

/// Clinical alert thresholds -> Observation.interpretation + real-time alerts.
pub const TEMP_HIGH: f64 = 38.0;
pub const TEMP_LOW: f64 = 35.0;
pub const SYS_HIGH: f64 = 140.0;
pub const SYS_LOW: f64 = 90.0;
pub const DIA_HIGH: f64 = 90.0;
pub const DIA_LOW: f64 = 60.0;
