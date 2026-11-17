"""Terminology and clinical reference values (FHIR R4B Vital Signs profile)."""

LOINC = "http://loinc.org"
UCUM = "http://unitsofmeasure.org"
OBS_CATEGORY = "http://terminology.hl7.org/CodeSystem/observation-category"
INTERPRETATION = "http://terminology.hl7.org/CodeSystem/v3-ObservationInterpretation"

PROFILE_VITALSIGNS = "http://hl7.org/fhir/StructureDefinition/vitalsigns"
PROFILE_BODYTEMP = "http://hl7.org/fhir/StructureDefinition/bodytemp"
PROFILE_BP = "http://hl7.org/fhir/StructureDefinition/bp"

CODE_BODY_TEMP = "8310-5"
CODE_BP_PANEL = "85354-9"
CODE_SYSTOLIC = "8480-6"
CODE_DIASTOLIC = "8462-4"

DISPLAY = {
    CODE_BODY_TEMP: "Body temperature",
    CODE_BP_PANEL: "Blood pressure panel with all children optional",
    CODE_SYSTOLIC: "Systolic blood pressure",
    CODE_DIASTOLIC: "Diastolic blood pressure",
}

UNIT_CEL = "Cel"
UNIT_MMHG = "mm[Hg]"

# Observation.status value set (required binding)
OBS_STATUS = {
    "registered",
    "preliminary",
    "final",
    "amended",
    "corrected",
    "cancelled",
    "entered-in-error",
    "unknown",
}

# Physiologically *possible* ranges: outside these the reading is a sensor fault -> reject.
PLAUSIBLE = {
    "temp_c": (25.0, 45.0),
    "systolic": (40.0, 300.0),
    "diastolic": (20.0, 200.0),
}

# Clinical alert thresholds -> Observation.interpretation + real-time alerts.
TEMP_HIGH = 38.0
TEMP_LOW = 35.0
SYS_HIGH = 140.0
SYS_LOW = 90.0
DIA_HIGH = 90.0
DIA_LOW = 60.0
