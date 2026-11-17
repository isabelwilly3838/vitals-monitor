-- FHIR AuditEvent trail (append-only from the application's point of view).
CREATE TABLE audit_events (
    id        UUID        PRIMARY KEY,
    recorded  TIMESTAMPTZ NOT NULL,
    username  TEXT        NOT NULL,
    action    CHAR(1)     NOT NULL,
    outcome   TEXT        NOT NULL,
    data      BYTEA       NOT NULL
);
CREATE INDEX ix_audit_recorded ON audit_events (recorded DESC);
