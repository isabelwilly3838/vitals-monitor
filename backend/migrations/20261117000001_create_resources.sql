-- FHIR resources. Only non-sensitive index columns are clear text; the full FHIR JSON
-- is AES-256-GCM encrypted by the application (column `data`).
CREATE TABLE resources (
    rtype       TEXT        NOT NULL,
    id          TEXT        NOT NULL,
    patient_id  TEXT,
    code        TEXT,
    effective   TIMESTAMPTZ,
    data        BYTEA       NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (rtype, id),
    CONSTRAINT fhir_id CHECK (id ~ '^[A-Za-z0-9\-.]{1,64}$')
);
CREATE INDEX ix_obs_search ON resources (rtype, patient_id, code, effective DESC);
