-- Freeze the one background job that would rewrite a seeded snapshot the fixtures depend on.
--
-- Migrations 005/028/029/030 register `refresh_almanac_cache` on an hourly schedule and run it
-- once on the way past. That single run is what the almanac fixtures recorded: it happened while
-- only the two emulator-driven stations had data, which is why almanac-populated carries eight
-- metrics for davis-full and no air-quality metrics at all -- /almanac reads the extremes of the
-- website's air quality device, and aqmon had not been seeded yet.
--
-- Leaving the job scheduled makes that a race. Fire it after contract/tools/rig/seed.sql and
-- aqmon gains high_pm25 / high_pm10_in / high_co2 / high_tvoc, so /almanac?station=davis-full
-- grows four keys the fixture does not have and the suite fails. Whether it fires depends on
-- where in the hour CI happened to start, which is the definition of a flaky gate.
--
-- The other provisioned jobs are deliberately left alone: refresh_snow_cache and
-- update_rainfall_summary both feed values, never key sets, and /snow is written to fall back to
-- its SQL functions when the cache misses -- so the response shape is the same either way, and
-- stopping them would diverge from the server the fixtures were captured from.
SELECT delete_job(job_id)
  FROM timescaledb_information.jobs
 WHERE proc_name = 'refresh_almanac_cache';
