-- The activation boundary and observed pending orders survive restarts, so
-- historical fills stay quiet while pre-existing resting orders can still ding.
CREATE TABLE operator_notification_markers (
    marker_key text PRIMARY KEY,
    observed_at timestamptz NOT NULL DEFAULT now()
);
