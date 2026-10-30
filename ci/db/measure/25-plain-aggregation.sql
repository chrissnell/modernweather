-- Measurement 3b: the identical aggregation as a plain query, which is the control
-- DESIGN.md D-17's 125x claim is relative to. Generated from
-- generated/sql/030_observations_1m.sql by ci/db/measure/make-plain-query.py so it cannot
-- drift from the view it is the control for.
\timing on
SELECT count(*) FROM (
SELECT
    time_bucket(INTERVAL '1 minute', time) AS bucket,
    station_id,
    -- Pressure [barometer]
    sum(barometer::double precision) AS barometer_sum,
    nullif(count(barometer), 0) AS barometer_count,
    min(barometer) AS barometer_min,
    max(barometer) AS barometer_max,
    -- Indoor temperature [in_temp]
    sum(intemp::double precision) AS intemp_sum,
    nullif(count(intemp), 0) AS intemp_count,
    min(intemp) AS intemp_min,
    max(intemp) AS intemp_max,
    -- Indoor humidity [in_humidity]
    sum(inhumidity::double precision) AS inhumidity_sum,
    nullif(count(inhumidity), 0) AS inhumidity_count,
    min(inhumidity) AS inhumidity_min,
    max(inhumidity) AS inhumidity_max,
    -- Temperature [out_temp]
    sum(outtemp::double precision) AS outtemp_sum,
    nullif(count(outtemp), 0) AS outtemp_count,
    min(outtemp) AS outtemp_min,
    max(outtemp) AS outtemp_max,
    -- Humidity [out_humidity]
    sum(outhumidity::double precision) AS outhumidity_sum,
    nullif(count(outhumidity), 0) AS outhumidity_count,
    min(outhumidity) AS outhumidity_min,
    max(outhumidity) AS outhumidity_max,
    -- Wind speed [wind_speed]
    sum(windspeed::double precision) AS windspeed_sum,
    nullif(count(windspeed), 0) AS windspeed_count,
    max(windspeed) AS windspeed_max,
    -- Wind speed (10-minute average) [wind_speed10]
    sum(windspeed10::double precision) AS windspeed10_sum,
    nullif(count(windspeed10), 0) AS windspeed10_count,
    max(windspeed10) AS windspeed10_max,
    -- Wind direction [wind_dir]
    sum(sind(winddir::double precision)) AS winddir_sin_sum,
    sum(cosd(winddir::double precision)) AS winddir_cos_sum,
    nullif(count(winddir), 0) AS winddir_dir_count,
    -- Wind chill [wind_chill]
    sum(windchill::double precision) AS windchill_sum,
    nullif(count(windchill), 0) AS windchill_count,
    min(windchill) AS windchill_min,
    -- Heat index [heat_index]
    sum(heatindex::double precision) AS heatindex_sum,
    nullif(count(heatindex), 0) AS heatindex_count,
    max(heatindex) AS heatindex_max,
    -- Extra temperature 1 [extra_temp1]
    sum(extratemp1::double precision) AS extratemp1_sum,
    nullif(count(extratemp1), 0) AS extratemp1_count,
    min(extratemp1) AS extratemp1_min,
    max(extratemp1) AS extratemp1_max,
    -- Extra temperature 2 [extra_temp2]
    sum(extratemp2::double precision) AS extratemp2_sum,
    nullif(count(extratemp2), 0) AS extratemp2_count,
    min(extratemp2) AS extratemp2_min,
    max(extratemp2) AS extratemp2_max,
    -- Extra temperature 3 [extra_temp3]
    sum(extratemp3::double precision) AS extratemp3_sum,
    nullif(count(extratemp3), 0) AS extratemp3_count,
    min(extratemp3) AS extratemp3_min,
    max(extratemp3) AS extratemp3_max,
    -- Extra temperature 4 [extra_temp4]
    sum(extratemp4::double precision) AS extratemp4_sum,
    nullif(count(extratemp4), 0) AS extratemp4_count,
    min(extratemp4) AS extratemp4_min,
    max(extratemp4) AS extratemp4_max,
    -- Extra temperature 5 [extra_temp5]
    sum(extratemp5::double precision) AS extratemp5_sum,
    nullif(count(extratemp5), 0) AS extratemp5_count,
    min(extratemp5) AS extratemp5_min,
    max(extratemp5) AS extratemp5_max,
    -- Extra temperature 6 [extra_temp6]
    sum(extratemp6::double precision) AS extratemp6_sum,
    nullif(count(extratemp6), 0) AS extratemp6_count,
    min(extratemp6) AS extratemp6_min,
    max(extratemp6) AS extratemp6_max,
    -- Extra temperature 7 [extra_temp7]
    sum(extratemp7::double precision) AS extratemp7_sum,
    nullif(count(extratemp7), 0) AS extratemp7_count,
    min(extratemp7) AS extratemp7_min,
    max(extratemp7) AS extratemp7_max,
    -- Soil temperature 1 [soil_temp1]
    sum(soiltemp1::double precision) AS soiltemp1_sum,
    nullif(count(soiltemp1), 0) AS soiltemp1_count,
    min(soiltemp1) AS soiltemp1_min,
    max(soiltemp1) AS soiltemp1_max,
    -- Soil temperature 2 [soil_temp2]
    sum(soiltemp2::double precision) AS soiltemp2_sum,
    nullif(count(soiltemp2), 0) AS soiltemp2_count,
    min(soiltemp2) AS soiltemp2_min,
    max(soiltemp2) AS soiltemp2_max,
    -- Soil temperature 3 [soil_temp3]
    sum(soiltemp3::double precision) AS soiltemp3_sum,
    nullif(count(soiltemp3), 0) AS soiltemp3_count,
    min(soiltemp3) AS soiltemp3_min,
    max(soiltemp3) AS soiltemp3_max,
    -- Soil temperature 4 [soil_temp4]
    sum(soiltemp4::double precision) AS soiltemp4_sum,
    nullif(count(soiltemp4), 0) AS soiltemp4_count,
    min(soiltemp4) AS soiltemp4_min,
    max(soiltemp4) AS soiltemp4_max,
    -- Leaf temperature 1 [leaf_temp1]
    sum(leaftemp1::double precision) AS leaftemp1_sum,
    nullif(count(leaftemp1), 0) AS leaftemp1_count,
    min(leaftemp1) AS leaftemp1_min,
    max(leaftemp1) AS leaftemp1_max,
    -- Leaf temperature 2 [leaf_temp2]
    sum(leaftemp2::double precision) AS leaftemp2_sum,
    nullif(count(leaftemp2), 0) AS leaftemp2_count,
    min(leaftemp2) AS leaftemp2_min,
    max(leaftemp2) AS leaftemp2_max,
    -- Leaf temperature 3 [leaf_temp3]
    sum(leaftemp3::double precision) AS leaftemp3_sum,
    nullif(count(leaftemp3), 0) AS leaftemp3_count,
    min(leaftemp3) AS leaftemp3_min,
    max(leaftemp3) AS leaftemp3_max,
    -- Leaf temperature 4 [leaf_temp4]
    sum(leaftemp4::double precision) AS leaftemp4_sum,
    nullif(count(leaftemp4), 0) AS leaftemp4_count,
    min(leaftemp4) AS leaftemp4_min,
    max(leaftemp4) AS leaftemp4_max,
    -- Extra humidity 1 [extra_humidity1]
    sum(extrahumidity1::double precision) AS extrahumidity1_sum,
    nullif(count(extrahumidity1), 0) AS extrahumidity1_count,
    min(extrahumidity1) AS extrahumidity1_min,
    max(extrahumidity1) AS extrahumidity1_max,
    -- Extra humidity 2 [extra_humidity2]
    sum(extrahumidity2::double precision) AS extrahumidity2_sum,
    nullif(count(extrahumidity2), 0) AS extrahumidity2_count,
    min(extrahumidity2) AS extrahumidity2_min,
    max(extrahumidity2) AS extrahumidity2_max,
    -- Extra humidity 3 [extra_humidity3]
    sum(extrahumidity3::double precision) AS extrahumidity3_sum,
    nullif(count(extrahumidity3), 0) AS extrahumidity3_count,
    min(extrahumidity3) AS extrahumidity3_min,
    max(extrahumidity3) AS extrahumidity3_max,
    -- Extra humidity 4 [extra_humidity4]
    sum(extrahumidity4::double precision) AS extrahumidity4_sum,
    nullif(count(extrahumidity4), 0) AS extrahumidity4_count,
    min(extrahumidity4) AS extrahumidity4_min,
    max(extrahumidity4) AS extrahumidity4_max,
    -- Extra humidity 5 [extra_humidity5]
    sum(extrahumidity5::double precision) AS extrahumidity5_sum,
    nullif(count(extrahumidity5), 0) AS extrahumidity5_count,
    min(extrahumidity5) AS extrahumidity5_min,
    max(extrahumidity5) AS extrahumidity5_max,
    -- Extra humidity 6 [extra_humidity6]
    sum(extrahumidity6::double precision) AS extrahumidity6_sum,
    nullif(count(extrahumidity6), 0) AS extrahumidity6_count,
    min(extrahumidity6) AS extrahumidity6_min,
    max(extrahumidity6) AS extrahumidity6_max,
    -- Extra humidity 7 [extra_humidity7]
    sum(extrahumidity7::double precision) AS extrahumidity7_sum,
    nullif(count(extrahumidity7), 0) AS extrahumidity7_count,
    min(extrahumidity7) AS extrahumidity7_min,
    max(extrahumidity7) AS extrahumidity7_max,
    -- Rain rate [rain_rate]
    sum(rainrate::double precision) AS rainrate_sum,
    nullif(count(rainrate), 0) AS rainrate_count,
    max(rainrate) AS rainrate_max,
    -- Rainfall since last reading [rain_incremental]
    sum(rainincremental::double precision) AS rainincremental_total,
    -- Storm rainfall [storm_rain]
    max(stormrain) AS stormrain_max,
    -- Rainfall today [day_rain]
    max(dayrain) AS dayrain_max,
    -- Rainfall this month [month_rain]
    max(monthrain) AS monthrain_max,
    -- Rainfall this year [year_rain]
    max(yearrain) AS yearrain_max,
    -- Solar radiation [solar_watts]
    sum(solarwatts::double precision) AS solarwatts_sum,
    nullif(count(solarwatts), 0) AS solarwatts_count,
    min(solarwatts) AS solarwatts_min,
    max(solarwatts) AS solarwatts_max,
    -- Clear-sky solar radiation [potential_solar_watts]
    sum(potentialsolarwatts::double precision) AS potentialsolarwatts_sum,
    nullif(count(potentialsolarwatts), 0) AS potentialsolarwatts_count,
    min(potentialsolarwatts) AS potentialsolarwatts_min,
    max(potentialsolarwatts) AS potentialsolarwatts_max,
    -- Solar energy [solar_joules]
    sum(solarjoules::double precision) AS solarjoules_sum,
    nullif(count(solarjoules), 0) AS solarjoules_count,
    min(solarjoules) AS solarjoules_min,
    max(solarjoules) AS solarjoules_max,
    -- UV index [uv]
    sum(uv::double precision) AS uv_sum,
    nullif(count(uv), 0) AS uv_count,
    min(uv) AS uv_min,
    max(uv) AS uv_max,
    -- Solar radiation (sensor) [radiation]
    sum(radiation::double precision) AS radiation_sum,
    nullif(count(radiation), 0) AS radiation_count,
    min(radiation) AS radiation_min,
    max(radiation) AS radiation_max,
    -- Evapotranspiration today [day_et]
    max(dayet) AS dayet_max,
    -- Evapotranspiration this month [month_et]
    max(monthet) AS monthet_max,
    -- Evapotranspiration this year [year_et]
    max(yearet) AS yearet_max,
    -- Soil moisture 1 [soil_moisture1]
    sum(soilmoisture1::double precision) AS soilmoisture1_sum,
    nullif(count(soilmoisture1), 0) AS soilmoisture1_count,
    min(soilmoisture1) AS soilmoisture1_min,
    max(soilmoisture1) AS soilmoisture1_max,
    -- Soil moisture 2 [soil_moisture2]
    sum(soilmoisture2::double precision) AS soilmoisture2_sum,
    nullif(count(soilmoisture2), 0) AS soilmoisture2_count,
    min(soilmoisture2) AS soilmoisture2_min,
    max(soilmoisture2) AS soilmoisture2_max,
    -- Soil moisture 3 [soil_moisture3]
    sum(soilmoisture3::double precision) AS soilmoisture3_sum,
    nullif(count(soilmoisture3), 0) AS soilmoisture3_count,
    min(soilmoisture3) AS soilmoisture3_min,
    max(soilmoisture3) AS soilmoisture3_max,
    -- Soil moisture 4 [soil_moisture4]
    sum(soilmoisture4::double precision) AS soilmoisture4_sum,
    nullif(count(soilmoisture4), 0) AS soilmoisture4_count,
    min(soilmoisture4) AS soilmoisture4_min,
    max(soilmoisture4) AS soilmoisture4_max,
    -- Leaf wetness 1 [leaf_wetness1]
    sum(leafwetness1::double precision) AS leafwetness1_sum,
    nullif(count(leafwetness1), 0) AS leafwetness1_count,
    min(leafwetness1) AS leafwetness1_min,
    max(leafwetness1) AS leafwetness1_max,
    -- Leaf wetness 2 [leaf_wetness2]
    sum(leafwetness2::double precision) AS leafwetness2_sum,
    nullif(count(leafwetness2), 0) AS leafwetness2_count,
    min(leafwetness2) AS leafwetness2_min,
    max(leafwetness2) AS leafwetness2_max,
    -- Leaf wetness 3 [leaf_wetness3]
    sum(leafwetness3::double precision) AS leafwetness3_sum,
    nullif(count(leafwetness3), 0) AS leafwetness3_count,
    min(leafwetness3) AS leafwetness3_min,
    max(leafwetness3) AS leafwetness3_max,
    -- Leaf wetness 4 [leaf_wetness4]
    sum(leafwetness4::double precision) AS leafwetness4_sum,
    nullif(count(leafwetness4), 0) AS leafwetness4_count,
    min(leafwetness4) AS leafwetness4_min,
    max(leafwetness4) AS leafwetness4_max,
    -- Indoor alarm [inside_alarm]
    min(insidealarm) AS insidealarm_min,
    max(insidealarm) AS insidealarm_max,
    -- Rain alarm [rain_alarm]
    min(rainalarm) AS rainalarm_min,
    max(rainalarm) AS rainalarm_max,
    -- Outside alarm 1 [outside_alarm1]
    min(outsidealarm1) AS outsidealarm1_min,
    max(outsidealarm1) AS outsidealarm1_max,
    -- Outside alarm 2 [outside_alarm2]
    min(outsidealarm2) AS outsidealarm2_min,
    max(outsidealarm2) AS outsidealarm2_max,
    -- Extra alarm 1 [extra_alarm1]
    min(extraalarm1) AS extraalarm1_min,
    max(extraalarm1) AS extraalarm1_max,
    -- Extra alarm 2 [extra_alarm2]
    min(extraalarm2) AS extraalarm2_min,
    max(extraalarm2) AS extraalarm2_max,
    -- Extra alarm 3 [extra_alarm3]
    min(extraalarm3) AS extraalarm3_min,
    max(extraalarm3) AS extraalarm3_max,
    -- Extra alarm 4 [extra_alarm4]
    min(extraalarm4) AS extraalarm4_min,
    max(extraalarm4) AS extraalarm4_max,
    -- Extra alarm 5 [extra_alarm5]
    min(extraalarm5) AS extraalarm5_min,
    max(extraalarm5) AS extraalarm5_max,
    -- Extra alarm 6 [extra_alarm6]
    min(extraalarm6) AS extraalarm6_min,
    max(extraalarm6) AS extraalarm6_max,
    -- Extra alarm 7 [extra_alarm7]
    min(extraalarm7) AS extraalarm7_min,
    max(extraalarm7) AS extraalarm7_max,
    -- Extra alarm 8 [extra_alarm8]
    min(extraalarm8) AS extraalarm8_min,
    max(extraalarm8) AS extraalarm8_max,
    -- Soil/leaf alarm 1 [soil_leaf_alarm1]
    min(soilleafalarm1) AS soilleafalarm1_min,
    max(soilleafalarm1) AS soilleafalarm1_max,
    -- Soil/leaf alarm 2 [soil_leaf_alarm2]
    min(soilleafalarm2) AS soilleafalarm2_min,
    max(soilleafalarm2) AS soilleafalarm2_max,
    -- Soil/leaf alarm 3 [soil_leaf_alarm3]
    min(soilleafalarm3) AS soilleafalarm3_min,
    max(soilleafalarm3) AS soilleafalarm3_max,
    -- Soil/leaf alarm 4 [soil_leaf_alarm4]
    min(soilleafalarm4) AS soilleafalarm4_min,
    max(soilleafalarm4) AS soilleafalarm4_max,
    -- Transmitter battery [tx_battery_status]
    min(txbatterystatus) AS txbatterystatus_min,
    max(txbatterystatus) AS txbatterystatus_max,
    -- Console battery voltage [cons_battery_voltage]
    sum(consbatteryvoltage::double precision) AS consbatteryvoltage_sum,
    nullif(count(consbatteryvoltage), 0) AS consbatteryvoltage_count,
    min(consbatteryvoltage) AS consbatteryvoltage_min,
    max(consbatteryvoltage) AS consbatteryvoltage_max,
    -- Station battery voltage [station_battery_voltage]
    sum(stationbatteryvoltage::double precision) AS stationbatteryvoltage_sum,
    nullif(count(stationbatteryvoltage), 0) AS stationbatteryvoltage_count,
    min(stationbatteryvoltage) AS stationbatteryvoltage_min,
    max(stationbatteryvoltage) AS stationbatteryvoltage_max,
    -- Snow sensor distance [snow_distance]
    sum(snowdistance::double precision) AS snowdistance_sum,
    nullif(count(snowdistance), 0) AS snowdistance_count,
    min(snowdistance) AS snowdistance_min,
    max(snowdistance) AS snowdistance_max,
    -- Snow depth [snow_depth]
    sum(snowdepth::double precision) AS snowdepth_sum,
    nullif(count(snowdepth), 0) AS snowdepth_count,
    min(snowdepth) AS snowdepth_min,
    max(snowdepth) AS snowdepth_max,
    -- Extra value 1 [extra_float1]
    sum(extrafloat1::double precision) AS extrafloat1_sum,
    nullif(count(extrafloat1), 0) AS extrafloat1_count,
    min(extrafloat1) AS extrafloat1_min,
    max(extrafloat1) AS extrafloat1_max,
    -- Extra value 2 [extra_float2]
    sum(extrafloat2::double precision) AS extrafloat2_sum,
    nullif(count(extrafloat2), 0) AS extrafloat2_count,
    min(extrafloat2) AS extrafloat2_min,
    max(extrafloat2) AS extrafloat2_max,
    -- Extra value 3 [extra_float3]
    sum(extrafloat3::double precision) AS extrafloat3_sum,
    nullif(count(extrafloat3), 0) AS extrafloat3_count,
    min(extrafloat3) AS extrafloat3_min,
    max(extrafloat3) AS extrafloat3_max,
    -- Extra value 4 [extra_float4]
    sum(extrafloat4::double precision) AS extrafloat4_sum,
    nullif(count(extrafloat4), 0) AS extrafloat4_count,
    min(extrafloat4) AS extrafloat4_min,
    max(extrafloat4) AS extrafloat4_max,
    -- Extra value 5 [extra_float5]
    sum(extrafloat5::double precision) AS extrafloat5_sum,
    nullif(count(extrafloat5), 0) AS extrafloat5_count,
    min(extrafloat5) AS extrafloat5_min,
    max(extrafloat5) AS extrafloat5_max,
    -- Extra value 6 [extra_float6]
    sum(extrafloat6::double precision) AS extrafloat6_sum,
    nullif(count(extrafloat6), 0) AS extrafloat6_count,
    min(extrafloat6) AS extrafloat6_min,
    max(extrafloat6) AS extrafloat6_max,
    -- Extra value 7 [extra_float7]
    sum(extrafloat7::double precision) AS extrafloat7_sum,
    nullif(count(extrafloat7), 0) AS extrafloat7_count,
    min(extrafloat7) AS extrafloat7_min,
    max(extrafloat7) AS extrafloat7_max,
    -- Extra value 8 [extra_float8]
    sum(extrafloat8::double precision) AS extrafloat8_sum,
    nullif(count(extrafloat8), 0) AS extrafloat8_count,
    min(extrafloat8) AS extrafloat8_min,
    max(extrafloat8) AS extrafloat8_max,
    -- Extra value 9 [extra_float9]
    sum(extrafloat9::double precision) AS extrafloat9_sum,
    nullif(count(extrafloat9), 0) AS extrafloat9_count,
    min(extrafloat9) AS extrafloat9_min,
    max(extrafloat9) AS extrafloat9_max,
    -- Extra value 10 [extra_float10]
    sum(extrafloat10::double precision) AS extrafloat10_sum,
    nullif(count(extrafloat10), 0) AS extrafloat10_count,
    min(extrafloat10) AS extrafloat10_min,
    max(extrafloat10) AS extrafloat10_max,
    -- Temperature sensor 1 [temp1]
    sum(temp1::double precision) AS temp1_sum,
    nullif(count(temp1), 0) AS temp1_count,
    min(temp1) AS temp1_min,
    max(temp1) AS temp1_max,
    -- Temperature sensor 2 [temp2]
    sum(temp2::double precision) AS temp2_sum,
    nullif(count(temp2), 0) AS temp2_count,
    min(temp2) AS temp2_min,
    max(temp2) AS temp2_max,
    -- Temperature sensor 3 [temp3]
    sum(temp3::double precision) AS temp3_sum,
    nullif(count(temp3), 0) AS temp3_count,
    min(temp3) AS temp3_min,
    max(temp3) AS temp3_max,
    -- Temperature sensor 4 [temp4]
    sum(temp4::double precision) AS temp4_sum,
    nullif(count(temp4), 0) AS temp4_count,
    min(temp4) AS temp4_min,
    max(temp4) AS temp4_max,
    -- Temperature sensor 5 [temp5]
    sum(temp5::double precision) AS temp5_sum,
    nullif(count(temp5), 0) AS temp5_count,
    min(temp5) AS temp5_min,
    max(temp5) AS temp5_max,
    -- Temperature sensor 6 [temp6]
    sum(temp6::double precision) AS temp6_sum,
    nullif(count(temp6), 0) AS temp6_count,
    min(temp6) AS temp6_min,
    max(temp6) AS temp6_max,
    -- Temperature sensor 7 [temp7]
    sum(temp7::double precision) AS temp7_sum,
    nullif(count(temp7), 0) AS temp7_count,
    min(temp7) AS temp7_min,
    max(temp7) AS temp7_max,
    -- Temperature sensor 8 [temp8]
    sum(temp8::double precision) AS temp8_sum,
    nullif(count(temp8), 0) AS temp8_count,
    min(temp8) AS temp8_min,
    max(temp8) AS temp8_max,
    -- Temperature sensor 9 [temp9]
    sum(temp9::double precision) AS temp9_sum,
    nullif(count(temp9), 0) AS temp9_count,
    min(temp9) AS temp9_min,
    max(temp9) AS temp9_max,
    -- Temperature sensor 10 [temp10]
    sum(temp10::double precision) AS temp10_sum,
    nullif(count(temp10), 0) AS temp10_count,
    min(temp10) AS temp10_min,
    max(temp10) AS temp10_max,
    -- Soil temperature 5 [soil_temp5]
    sum(soiltemp5::double precision) AS soiltemp5_sum,
    nullif(count(soiltemp5), 0) AS soiltemp5_count,
    min(soiltemp5) AS soiltemp5_min,
    max(soiltemp5) AS soiltemp5_max,
    -- Soil temperature 6 [soil_temp6]
    sum(soiltemp6::double precision) AS soiltemp6_sum,
    nullif(count(soiltemp6), 0) AS soiltemp6_count,
    min(soiltemp6) AS soiltemp6_min,
    max(soiltemp6) AS soiltemp6_max,
    -- Soil temperature 7 [soil_temp7]
    sum(soiltemp7::double precision) AS soiltemp7_sum,
    nullif(count(soiltemp7), 0) AS soiltemp7_count,
    min(soiltemp7) AS soiltemp7_min,
    max(soiltemp7) AS soiltemp7_max,
    -- Soil temperature 8 [soil_temp8]
    sum(soiltemp8::double precision) AS soiltemp8_sum,
    nullif(count(soiltemp8), 0) AS soiltemp8_count,
    min(soiltemp8) AS soiltemp8_min,
    max(soiltemp8) AS soiltemp8_max,
    -- Soil temperature 9 [soil_temp9]
    sum(soiltemp9::double precision) AS soiltemp9_sum,
    nullif(count(soiltemp9), 0) AS soiltemp9_count,
    min(soiltemp9) AS soiltemp9_min,
    max(soiltemp9) AS soiltemp9_max,
    -- Soil temperature 10 [soil_temp10]
    sum(soiltemp10::double precision) AS soiltemp10_sum,
    nullif(count(soiltemp10), 0) AS soiltemp10_count,
    min(soiltemp10) AS soiltemp10_min,
    max(soiltemp10) AS soiltemp10_max,
    -- Humidity sensor 1 [humidity1]
    sum(humidity1::double precision) AS humidity1_sum,
    nullif(count(humidity1), 0) AS humidity1_count,
    min(humidity1) AS humidity1_min,
    max(humidity1) AS humidity1_max,
    -- Humidity sensor 2 [humidity2]
    sum(humidity2::double precision) AS humidity2_sum,
    nullif(count(humidity2), 0) AS humidity2_count,
    min(humidity2) AS humidity2_min,
    max(humidity2) AS humidity2_max,
    -- Humidity sensor 3 [humidity3]
    sum(humidity3::double precision) AS humidity3_sum,
    nullif(count(humidity3), 0) AS humidity3_count,
    min(humidity3) AS humidity3_min,
    max(humidity3) AS humidity3_max,
    -- Humidity sensor 4 [humidity4]
    sum(humidity4::double precision) AS humidity4_sum,
    nullif(count(humidity4), 0) AS humidity4_count,
    min(humidity4) AS humidity4_min,
    max(humidity4) AS humidity4_max,
    -- Humidity sensor 5 [humidity5]
    sum(humidity5::double precision) AS humidity5_sum,
    nullif(count(humidity5), 0) AS humidity5_count,
    min(humidity5) AS humidity5_min,
    max(humidity5) AS humidity5_max,
    -- Humidity sensor 6 [humidity6]
    sum(humidity6::double precision) AS humidity6_sum,
    nullif(count(humidity6), 0) AS humidity6_count,
    min(humidity6) AS humidity6_min,
    max(humidity6) AS humidity6_max,
    -- Humidity sensor 7 [humidity7]
    sum(humidity7::double precision) AS humidity7_sum,
    nullif(count(humidity7), 0) AS humidity7_count,
    min(humidity7) AS humidity7_min,
    max(humidity7) AS humidity7_max,
    -- Humidity sensor 8 [humidity8]
    sum(humidity8::double precision) AS humidity8_sum,
    nullif(count(humidity8), 0) AS humidity8_count,
    min(humidity8) AS humidity8_min,
    max(humidity8) AS humidity8_max,
    -- Humidity sensor 9 [humidity9]
    sum(humidity9::double precision) AS humidity9_sum,
    nullif(count(humidity9), 0) AS humidity9_count,
    min(humidity9) AS humidity9_min,
    max(humidity9) AS humidity9_max,
    -- Humidity sensor 10 [humidity10]
    sum(humidity10::double precision) AS humidity10_sum,
    nullif(count(humidity10), 0) AS humidity10_count,
    min(humidity10) AS humidity10_min,
    max(humidity10) AS humidity10_max,
    -- Soil humidity 1 [soil_hum1]
    sum(soilhum1::double precision) AS soilhum1_sum,
    nullif(count(soilhum1), 0) AS soilhum1_count,
    min(soilhum1) AS soilhum1_min,
    max(soilhum1) AS soilhum1_max,
    -- Soil humidity 2 [soil_hum2]
    sum(soilhum2::double precision) AS soilhum2_sum,
    nullif(count(soilhum2), 0) AS soilhum2_count,
    min(soilhum2) AS soilhum2_min,
    max(soilhum2) AS soilhum2_max,
    -- Soil humidity 3 [soil_hum3]
    sum(soilhum3::double precision) AS soilhum3_sum,
    nullif(count(soilhum3), 0) AS soilhum3_count,
    min(soilhum3) AS soilhum3_min,
    max(soilhum3) AS soilhum3_max,
    -- Soil humidity 4 [soil_hum4]
    sum(soilhum4::double precision) AS soilhum4_sum,
    nullif(count(soilhum4), 0) AS soilhum4_count,
    min(soilhum4) AS soilhum4_min,
    max(soilhum4) AS soilhum4_max,
    -- Soil humidity 5 [soil_hum5]
    sum(soilhum5::double precision) AS soilhum5_sum,
    nullif(count(soilhum5), 0) AS soilhum5_count,
    min(soilhum5) AS soilhum5_min,
    max(soilhum5) AS soilhum5_max,
    -- Soil humidity 6 [soil_hum6]
    sum(soilhum6::double precision) AS soilhum6_sum,
    nullif(count(soilhum6), 0) AS soilhum6_count,
    min(soilhum6) AS soilhum6_min,
    max(soilhum6) AS soilhum6_max,
    -- Soil humidity 7 [soil_hum7]
    sum(soilhum7::double precision) AS soilhum7_sum,
    nullif(count(soilhum7), 0) AS soilhum7_count,
    min(soilhum7) AS soilhum7_min,
    max(soilhum7) AS soilhum7_max,
    -- Soil humidity 8 [soil_hum8]
    sum(soilhum8::double precision) AS soilhum8_sum,
    nullif(count(soilhum8), 0) AS soilhum8_count,
    min(soilhum8) AS soilhum8_min,
    max(soilhum8) AS soilhum8_max,
    -- Soil humidity 9 [soil_hum9]
    sum(soilhum9::double precision) AS soilhum9_sum,
    nullif(count(soilhum9), 0) AS soilhum9_count,
    min(soilhum9) AS soilhum9_min,
    max(soilhum9) AS soilhum9_max,
    -- Soil humidity 10 [soil_hum10]
    sum(soilhum10::double precision) AS soilhum10_sum,
    nullif(count(soilhum10), 0) AS soilhum10_count,
    min(soilhum10) AS soilhum10_min,
    max(soilhum10) AS soilhum10_max,
    -- Leaf wetness 5 [leaf_wetness5]
    sum(leafwetness5::double precision) AS leafwetness5_sum,
    nullif(count(leafwetness5), 0) AS leafwetness5_count,
    min(leafwetness5) AS leafwetness5_min,
    max(leafwetness5) AS leafwetness5_max,
    -- Leaf wetness 6 [leaf_wetness6]
    sum(leafwetness6::double precision) AS leafwetness6_sum,
    nullif(count(leafwetness6), 0) AS leafwetness6_count,
    min(leafwetness6) AS leafwetness6_min,
    max(leafwetness6) AS leafwetness6_max,
    -- Leaf wetness 7 [leaf_wetness7]
    sum(leafwetness7::double precision) AS leafwetness7_sum,
    nullif(count(leafwetness7), 0) AS leafwetness7_count,
    min(leafwetness7) AS leafwetness7_min,
    max(leafwetness7) AS leafwetness7_max,
    -- Leaf wetness 8 [leaf_wetness8]
    sum(leafwetness8::double precision) AS leafwetness8_sum,
    nullif(count(leafwetness8), 0) AS leafwetness8_count,
    min(leafwetness8) AS leafwetness8_min,
    max(leafwetness8) AS leafwetness8_max,
    -- Soil tension 1 [soil_tens1]
    sum(soiltens1::double precision) AS soiltens1_sum,
    nullif(count(soiltens1), 0) AS soiltens1_count,
    min(soiltens1) AS soiltens1_min,
    max(soiltens1) AS soiltens1_max,
    -- Soil tension 2 [soil_tens2]
    sum(soiltens2::double precision) AS soiltens2_sum,
    nullif(count(soiltens2), 0) AS soiltens2_count,
    min(soiltens2) AS soiltens2_min,
    max(soiltens2) AS soiltens2_max,
    -- Soil tension 3 [soil_tens3]
    sum(soiltens3::double precision) AS soiltens3_sum,
    nullif(count(soiltens3), 0) AS soiltens3_count,
    min(soiltens3) AS soiltens3_min,
    max(soiltens3) AS soiltens3_max,
    -- Soil tension 4 [soil_tens4]
    sum(soiltens4::double precision) AS soiltens4_sum,
    nullif(count(soiltens4), 0) AS soiltens4_count,
    min(soiltens4) AS soiltens4_min,
    max(soiltens4) AS soiltens4_max,
    -- Growing degree days [gdd]
    max(gdd) AS gdd_max,
    -- Reference evapotranspiration (short crop) [etos]
    sum(etos::double precision) AS etos_sum,
    nullif(count(etos), 0) AS etos_count,
    min(etos) AS etos_min,
    max(etos) AS etos_max,
    -- Reference evapotranspiration (tall crop) [etrs]
    sum(etrs::double precision) AS etrs_sum,
    nullif(count(etrs), 0) AS etrs_count,
    min(etrs) AS etrs_min,
    max(etrs) AS etrs_max,
    -- Leak detector 1 [leak1]
    min(leak1) AS leak1_min,
    max(leak1) AS leak1_max,
    -- Leak detector 2 [leak2]
    min(leak2) AS leak2_min,
    max(leak2) AS leak2_max,
    -- Leak detector 3 [leak3]
    min(leak3) AS leak3_min,
    max(leak3) AS leak3_max,
    -- Leak detector 4 [leak4]
    min(leak4) AS leak4_min,
    max(leak4) AS leak4_max,
    -- Outdoor sensor battery [batt_out]
    min(battout) AS battout_min,
    max(battout) AS battout_max,
    -- Indoor sensor battery [batt_in]
    min(battin) AS battin_min,
    max(battin) AS battin_max,
    -- Battery 1 [batt1]
    min(batt1) AS batt1_min,
    max(batt1) AS batt1_max,
    -- Battery 2 [batt2]
    min(batt2) AS batt2_min,
    max(batt2) AS batt2_max,
    -- Battery 3 [batt3]
    min(batt3) AS batt3_min,
    max(batt3) AS batt3_max,
    -- Battery 4 [batt4]
    min(batt4) AS batt4_min,
    max(batt4) AS batt4_max,
    -- Battery 5 [batt5]
    min(batt5) AS batt5_min,
    max(batt5) AS batt5_max,
    -- Battery 6 [batt6]
    min(batt6) AS batt6_min,
    max(batt6) AS batt6_max,
    -- Battery 7 [batt7]
    min(batt7) AS batt7_min,
    max(batt7) AS batt7_max,
    -- Battery 8 [batt8]
    min(batt8) AS batt8_min,
    max(batt8) AS batt8_max,
    -- Battery 9 [batt9]
    min(batt9) AS batt9_min,
    max(batt9) AS batt9_max,
    -- Battery 10 [batt10]
    min(batt10) AS batt10_min,
    max(batt10) AS batt10_max,
    -- Particulate sensor battery [batt_25]
    min(batt_25) AS batt_25_min,
    max(batt_25) AS batt_25_max,
    -- Lightning sensor battery [batt_lightning]
    min(batt_lightning) AS batt_lightning_min,
    max(batt_lightning) AS batt_lightning_max,
    -- Leak detector 1 battery [bat_leak1]
    min(batleak1) AS batleak1_min,
    max(batleak1) AS batleak1_max,
    -- Leak detector 2 battery [bat_leak2]
    min(batleak2) AS batleak2_min,
    max(batleak2) AS batleak2_max,
    -- Leak detector 3 battery [bat_leak3]
    min(batleak3) AS batleak3_min,
    max(batleak3) AS batleak3_max,
    -- Leak detector 4 battery [bat_leak4]
    min(batleak4) AS batleak4_min,
    max(batleak4) AS batleak4_max,
    -- Soil moisture sensor 1 battery [batt_sm1]
    min(battsm1) AS battsm1_min,
    max(battsm1) AS battsm1_max,
    -- Soil moisture sensor 2 battery [batt_sm2]
    min(battsm2) AS battsm2_min,
    max(battsm2) AS battsm2_max,
    -- Soil moisture sensor 3 battery [batt_sm3]
    min(battsm3) AS battsm3_min,
    max(battsm3) AS battsm3_max,
    -- Soil moisture sensor 4 battery [batt_sm4]
    min(battsm4) AS battsm4_min,
    max(battsm4) AS battsm4_max,
    -- CO2 sensor battery [batt_co2]
    min(batt_co2) AS batt_co2_min,
    max(batt_co2) AS batt_co2_max,
    -- Cellular gateway battery [batt_cellgateway]
    min(batt_cellgateway) AS batt_cellgateway_min,
    max(batt_cellgateway) AS batt_cellgateway_max,
    -- Pressure (relative) [barom_rel_in]
    sum(baromrelin::double precision) AS baromrelin_sum,
    nullif(count(baromrelin), 0) AS baromrelin_count,
    min(baromrelin) AS baromrelin_min,
    max(baromrelin) AS baromrelin_max,
    -- Pressure (absolute) [barom_abs_in]
    sum(baromabsin::double precision) AS baromabsin_sum,
    nullif(count(baromabsin), 0) AS baromabsin_count,
    min(baromabsin) AS baromabsin_min,
    max(baromabsin) AS baromabsin_max,
    -- Relay 1 [relay1]
    min(relay1) AS relay1_min,
    max(relay1) AS relay1_max,
    -- Relay 2 [relay2]
    min(relay2) AS relay2_min,
    max(relay2) AS relay2_max,
    -- Relay 3 [relay3]
    min(relay3) AS relay3_min,
    max(relay3) AS relay3_max,
    -- Relay 4 [relay4]
    min(relay4) AS relay4_min,
    max(relay4) AS relay4_max,
    -- Relay 5 [relay5]
    min(relay5) AS relay5_min,
    max(relay5) AS relay5_max,
    -- Relay 6 [relay6]
    min(relay6) AS relay6_min,
    max(relay6) AS relay6_max,
    -- Relay 7 [relay7]
    min(relay7) AS relay7_min,
    max(relay7) AS relay7_max,
    -- Relay 8 [relay8]
    min(relay8) AS relay8_min,
    max(relay8) AS relay8_max,
    -- Relay 9 [relay9]
    min(relay9) AS relay9_min,
    max(relay9) AS relay9_max,
    -- Relay 10 [relay10]
    min(relay10) AS relay10_min,
    max(relay10) AS relay10_max,
    -- PM2.5 [pm25]
    sum(pm25::double precision) AS pm25_sum,
    nullif(count(pm25), 0) AS pm25_count,
    min(pm25) AS pm25_min,
    max(pm25) AS pm25_max,
    -- PM2.5 (24-hour average) [pm25_24h]
    sum(pm25_24h::double precision) AS pm25_24h_sum,
    nullif(count(pm25_24h), 0) AS pm25_24h_count,
    min(pm25_24h) AS pm25_24h_min,
    max(pm25_24h) AS pm25_24h_max,
    -- PM2.5 indoors [pm25_in]
    sum(pm25_in::double precision) AS pm25_in_sum,
    nullif(count(pm25_in), 0) AS pm25_in_count,
    min(pm25_in) AS pm25_in_min,
    max(pm25_in) AS pm25_in_max,
    -- PM2.5 indoors (24-hour average) [pm25_in_24h]
    sum(pm25_in_24h::double precision) AS pm25_in_24h_sum,
    nullif(count(pm25_in_24h), 0) AS pm25_in_24h_count,
    min(pm25_in_24h) AS pm25_in_24h_min,
    max(pm25_in_24h) AS pm25_in_24h_max,
    -- PM2.5 indoors (AQIN sensor) [pm25_in_aqin]
    sum(pm25_in_aqin::double precision) AS pm25_in_aqin_sum,
    nullif(count(pm25_in_aqin), 0) AS pm25_in_aqin_count,
    min(pm25_in_aqin) AS pm25_in_aqin_min,
    max(pm25_in_aqin) AS pm25_in_aqin_max,
    -- PM2.5 indoors, 24-hour average (AQIN sensor) [pm25_in_24h_aqin]
    sum(pm25_in_24h_aqin::double precision) AS pm25_in_24h_aqin_sum,
    nullif(count(pm25_in_24h_aqin), 0) AS pm25_in_24h_aqin_count,
    min(pm25_in_24h_aqin) AS pm25_in_24h_aqin_min,
    max(pm25_in_24h_aqin) AS pm25_in_24h_aqin_max,
    -- PM10 indoors (AQIN sensor) [pm10_in_aqin]
    sum(pm10_in_aqin::double precision) AS pm10_in_aqin_sum,
    nullif(count(pm10_in_aqin), 0) AS pm10_in_aqin_count,
    min(pm10_in_aqin) AS pm10_in_aqin_min,
    max(pm10_in_aqin) AS pm10_in_aqin_max,
    -- PM10 indoors, 24-hour average (AQIN sensor) [pm10_in_24h_aqin]
    sum(pm10_in_24h_aqin::double precision) AS pm10_in_24h_aqin_sum,
    nullif(count(pm10_in_24h_aqin), 0) AS pm10_in_24h_aqin_count,
    min(pm10_in_24h_aqin) AS pm10_in_24h_aqin_min,
    max(pm10_in_24h_aqin) AS pm10_in_24h_aqin_max,
    -- CO2 [co2]
    sum(co2::double precision) AS co2_sum,
    nullif(count(co2), 0) AS co2_count,
    min(co2) AS co2_min,
    max(co2) AS co2_max,
    -- CO2 indoors (AQIN sensor) [co2_in_aqin]
    sum(co2_in_aqin::double precision) AS co2_in_aqin_sum,
    nullif(count(co2_in_aqin), 0) AS co2_in_aqin_count,
    min(co2_in_aqin) AS co2_in_aqin_min,
    max(co2_in_aqin) AS co2_in_aqin_max,
    -- CO2 indoors, 24-hour average (AQIN sensor) [co2_in_24h_aqin]
    sum(co2_in_24h_aqin::double precision) AS co2_in_24h_aqin_sum,
    nullif(count(co2_in_24h_aqin), 0) AS co2_in_24h_aqin_count,
    min(co2_in_24h_aqin) AS co2_in_24h_aqin_min,
    max(co2_in_24h_aqin) AS co2_in_24h_aqin_max,
    -- Indoor temperature (AQIN sensor) [pm_in_temp_aqin]
    sum(pm_in_temp_aqin::double precision) AS pm_in_temp_aqin_sum,
    nullif(count(pm_in_temp_aqin), 0) AS pm_in_temp_aqin_count,
    min(pm_in_temp_aqin) AS pm_in_temp_aqin_min,
    max(pm_in_temp_aqin) AS pm_in_temp_aqin_max,
    -- Indoor humidity (AQIN sensor) [pm_in_humidity_aqin]
    sum(pm_in_humidity_aqin::double precision) AS pm_in_humidity_aqin_sum,
    nullif(count(pm_in_humidity_aqin), 0) AS pm_in_humidity_aqin_count,
    min(pm_in_humidity_aqin) AS pm_in_humidity_aqin_min,
    max(pm_in_humidity_aqin) AS pm_in_humidity_aqin_max,
    -- Air quality index, PM2.5 (AQIN sensor) [aqi_pm25_aqin]
    sum(aqi_pm25_aqin::double precision) AS aqi_pm25_aqin_sum,
    nullif(count(aqi_pm25_aqin), 0) AS aqi_pm25_aqin_count,
    min(aqi_pm25_aqin) AS aqi_pm25_aqin_min,
    max(aqi_pm25_aqin) AS aqi_pm25_aqin_max,
    -- Air quality index, PM2.5 24-hour (AQIN sensor) [aqi_pm25_24h_aqin]
    sum(aqi_pm25_24h_aqin::double precision) AS aqi_pm25_24h_aqin_sum,
    nullif(count(aqi_pm25_24h_aqin), 0) AS aqi_pm25_24h_aqin_count,
    min(aqi_pm25_24h_aqin) AS aqi_pm25_24h_aqin_min,
    max(aqi_pm25_24h_aqin) AS aqi_pm25_24h_aqin_max,
    -- Air quality index, PM10 (AQIN sensor) [aqi_pm10_aqin]
    sum(aqi_pm10_aqin::double precision) AS aqi_pm10_aqin_sum,
    nullif(count(aqi_pm10_aqin), 0) AS aqi_pm10_aqin_count,
    min(aqi_pm10_aqin) AS aqi_pm10_aqin_min,
    max(aqi_pm10_aqin) AS aqi_pm10_aqin_max,
    -- Air quality index, PM10 24-hour (AQIN sensor) [aqi_pm10_24h_aqin]
    sum(aqi_pm10_24h_aqin::double precision) AS aqi_pm10_24h_aqin_sum,
    nullif(count(aqi_pm10_24h_aqin), 0) AS aqi_pm10_24h_aqin_count,
    min(aqi_pm10_24h_aqin) AS aqi_pm10_24h_aqin_min,
    max(aqi_pm10_24h_aqin) AS aqi_pm10_24h_aqin_max,
    -- Air quality index, PM2.5 indoors [aqi_pm25_in]
    sum(aqi_pm25_in::double precision) AS aqi_pm25_in_sum,
    nullif(count(aqi_pm25_in), 0) AS aqi_pm25_in_count,
    min(aqi_pm25_in) AS aqi_pm25_in_min,
    max(aqi_pm25_in) AS aqi_pm25_in_max,
    -- Air quality index, PM2.5 indoors 24-hour [aqi_pm25_in_24h]
    sum(aqi_pm25_in_24h::double precision) AS aqi_pm25_in_24h_sum,
    nullif(count(aqi_pm25_in_24h), 0) AS aqi_pm25_in_24h_count,
    min(aqi_pm25_in_24h) AS aqi_pm25_in_24h_min,
    max(aqi_pm25_in_24h) AS aqi_pm25_in_24h_max,
    -- Lightning strikes today [lightning_day]
    max(lightning_day) AS lightning_day_max,
    -- Lightning strikes this hour [lightning_hour]
    max(lightning_hour) AS lightning_hour_max,
    -- Distance to last lightning strike [lightning_distance]
    sum(lightning_distance::double precision) AS lightning_distance_sum,
    nullif(count(lightning_distance), 0) AS lightning_distance_count,
    min(lightning_distance) AS lightning_distance_min,
    max(lightning_distance) AS lightning_distance_max
FROM observations
GROUP BY 1, 2
) q;
\timing off

-- Same aggregation, parallelism disabled, so the comparison with the refresh (which the
-- spike reported as single-threaded) is apples to apples.
SET max_parallel_workers_per_gather = 0;
\timing on
SELECT count(*) FROM (
SELECT
    time_bucket(INTERVAL '1 minute', time) AS bucket,
    station_id,
    -- Pressure [barometer]
    sum(barometer::double precision) AS barometer_sum,
    nullif(count(barometer), 0) AS barometer_count,
    min(barometer) AS barometer_min,
    max(barometer) AS barometer_max,
    -- Indoor temperature [in_temp]
    sum(intemp::double precision) AS intemp_sum,
    nullif(count(intemp), 0) AS intemp_count,
    min(intemp) AS intemp_min,
    max(intemp) AS intemp_max,
    -- Indoor humidity [in_humidity]
    sum(inhumidity::double precision) AS inhumidity_sum,
    nullif(count(inhumidity), 0) AS inhumidity_count,
    min(inhumidity) AS inhumidity_min,
    max(inhumidity) AS inhumidity_max,
    -- Temperature [out_temp]
    sum(outtemp::double precision) AS outtemp_sum,
    nullif(count(outtemp), 0) AS outtemp_count,
    min(outtemp) AS outtemp_min,
    max(outtemp) AS outtemp_max,
    -- Humidity [out_humidity]
    sum(outhumidity::double precision) AS outhumidity_sum,
    nullif(count(outhumidity), 0) AS outhumidity_count,
    min(outhumidity) AS outhumidity_min,
    max(outhumidity) AS outhumidity_max,
    -- Wind speed [wind_speed]
    sum(windspeed::double precision) AS windspeed_sum,
    nullif(count(windspeed), 0) AS windspeed_count,
    max(windspeed) AS windspeed_max,
    -- Wind speed (10-minute average) [wind_speed10]
    sum(windspeed10::double precision) AS windspeed10_sum,
    nullif(count(windspeed10), 0) AS windspeed10_count,
    max(windspeed10) AS windspeed10_max,
    -- Wind direction [wind_dir]
    sum(sind(winddir::double precision)) AS winddir_sin_sum,
    sum(cosd(winddir::double precision)) AS winddir_cos_sum,
    nullif(count(winddir), 0) AS winddir_dir_count,
    -- Wind chill [wind_chill]
    sum(windchill::double precision) AS windchill_sum,
    nullif(count(windchill), 0) AS windchill_count,
    min(windchill) AS windchill_min,
    -- Heat index [heat_index]
    sum(heatindex::double precision) AS heatindex_sum,
    nullif(count(heatindex), 0) AS heatindex_count,
    max(heatindex) AS heatindex_max,
    -- Extra temperature 1 [extra_temp1]
    sum(extratemp1::double precision) AS extratemp1_sum,
    nullif(count(extratemp1), 0) AS extratemp1_count,
    min(extratemp1) AS extratemp1_min,
    max(extratemp1) AS extratemp1_max,
    -- Extra temperature 2 [extra_temp2]
    sum(extratemp2::double precision) AS extratemp2_sum,
    nullif(count(extratemp2), 0) AS extratemp2_count,
    min(extratemp2) AS extratemp2_min,
    max(extratemp2) AS extratemp2_max,
    -- Extra temperature 3 [extra_temp3]
    sum(extratemp3::double precision) AS extratemp3_sum,
    nullif(count(extratemp3), 0) AS extratemp3_count,
    min(extratemp3) AS extratemp3_min,
    max(extratemp3) AS extratemp3_max,
    -- Extra temperature 4 [extra_temp4]
    sum(extratemp4::double precision) AS extratemp4_sum,
    nullif(count(extratemp4), 0) AS extratemp4_count,
    min(extratemp4) AS extratemp4_min,
    max(extratemp4) AS extratemp4_max,
    -- Extra temperature 5 [extra_temp5]
    sum(extratemp5::double precision) AS extratemp5_sum,
    nullif(count(extratemp5), 0) AS extratemp5_count,
    min(extratemp5) AS extratemp5_min,
    max(extratemp5) AS extratemp5_max,
    -- Extra temperature 6 [extra_temp6]
    sum(extratemp6::double precision) AS extratemp6_sum,
    nullif(count(extratemp6), 0) AS extratemp6_count,
    min(extratemp6) AS extratemp6_min,
    max(extratemp6) AS extratemp6_max,
    -- Extra temperature 7 [extra_temp7]
    sum(extratemp7::double precision) AS extratemp7_sum,
    nullif(count(extratemp7), 0) AS extratemp7_count,
    min(extratemp7) AS extratemp7_min,
    max(extratemp7) AS extratemp7_max,
    -- Soil temperature 1 [soil_temp1]
    sum(soiltemp1::double precision) AS soiltemp1_sum,
    nullif(count(soiltemp1), 0) AS soiltemp1_count,
    min(soiltemp1) AS soiltemp1_min,
    max(soiltemp1) AS soiltemp1_max,
    -- Soil temperature 2 [soil_temp2]
    sum(soiltemp2::double precision) AS soiltemp2_sum,
    nullif(count(soiltemp2), 0) AS soiltemp2_count,
    min(soiltemp2) AS soiltemp2_min,
    max(soiltemp2) AS soiltemp2_max,
    -- Soil temperature 3 [soil_temp3]
    sum(soiltemp3::double precision) AS soiltemp3_sum,
    nullif(count(soiltemp3), 0) AS soiltemp3_count,
    min(soiltemp3) AS soiltemp3_min,
    max(soiltemp3) AS soiltemp3_max,
    -- Soil temperature 4 [soil_temp4]
    sum(soiltemp4::double precision) AS soiltemp4_sum,
    nullif(count(soiltemp4), 0) AS soiltemp4_count,
    min(soiltemp4) AS soiltemp4_min,
    max(soiltemp4) AS soiltemp4_max,
    -- Leaf temperature 1 [leaf_temp1]
    sum(leaftemp1::double precision) AS leaftemp1_sum,
    nullif(count(leaftemp1), 0) AS leaftemp1_count,
    min(leaftemp1) AS leaftemp1_min,
    max(leaftemp1) AS leaftemp1_max,
    -- Leaf temperature 2 [leaf_temp2]
    sum(leaftemp2::double precision) AS leaftemp2_sum,
    nullif(count(leaftemp2), 0) AS leaftemp2_count,
    min(leaftemp2) AS leaftemp2_min,
    max(leaftemp2) AS leaftemp2_max,
    -- Leaf temperature 3 [leaf_temp3]
    sum(leaftemp3::double precision) AS leaftemp3_sum,
    nullif(count(leaftemp3), 0) AS leaftemp3_count,
    min(leaftemp3) AS leaftemp3_min,
    max(leaftemp3) AS leaftemp3_max,
    -- Leaf temperature 4 [leaf_temp4]
    sum(leaftemp4::double precision) AS leaftemp4_sum,
    nullif(count(leaftemp4), 0) AS leaftemp4_count,
    min(leaftemp4) AS leaftemp4_min,
    max(leaftemp4) AS leaftemp4_max,
    -- Extra humidity 1 [extra_humidity1]
    sum(extrahumidity1::double precision) AS extrahumidity1_sum,
    nullif(count(extrahumidity1), 0) AS extrahumidity1_count,
    min(extrahumidity1) AS extrahumidity1_min,
    max(extrahumidity1) AS extrahumidity1_max,
    -- Extra humidity 2 [extra_humidity2]
    sum(extrahumidity2::double precision) AS extrahumidity2_sum,
    nullif(count(extrahumidity2), 0) AS extrahumidity2_count,
    min(extrahumidity2) AS extrahumidity2_min,
    max(extrahumidity2) AS extrahumidity2_max,
    -- Extra humidity 3 [extra_humidity3]
    sum(extrahumidity3::double precision) AS extrahumidity3_sum,
    nullif(count(extrahumidity3), 0) AS extrahumidity3_count,
    min(extrahumidity3) AS extrahumidity3_min,
    max(extrahumidity3) AS extrahumidity3_max,
    -- Extra humidity 4 [extra_humidity4]
    sum(extrahumidity4::double precision) AS extrahumidity4_sum,
    nullif(count(extrahumidity4), 0) AS extrahumidity4_count,
    min(extrahumidity4) AS extrahumidity4_min,
    max(extrahumidity4) AS extrahumidity4_max,
    -- Extra humidity 5 [extra_humidity5]
    sum(extrahumidity5::double precision) AS extrahumidity5_sum,
    nullif(count(extrahumidity5), 0) AS extrahumidity5_count,
    min(extrahumidity5) AS extrahumidity5_min,
    max(extrahumidity5) AS extrahumidity5_max,
    -- Extra humidity 6 [extra_humidity6]
    sum(extrahumidity6::double precision) AS extrahumidity6_sum,
    nullif(count(extrahumidity6), 0) AS extrahumidity6_count,
    min(extrahumidity6) AS extrahumidity6_min,
    max(extrahumidity6) AS extrahumidity6_max,
    -- Extra humidity 7 [extra_humidity7]
    sum(extrahumidity7::double precision) AS extrahumidity7_sum,
    nullif(count(extrahumidity7), 0) AS extrahumidity7_count,
    min(extrahumidity7) AS extrahumidity7_min,
    max(extrahumidity7) AS extrahumidity7_max,
    -- Rain rate [rain_rate]
    sum(rainrate::double precision) AS rainrate_sum,
    nullif(count(rainrate), 0) AS rainrate_count,
    max(rainrate) AS rainrate_max,
    -- Rainfall since last reading [rain_incremental]
    sum(rainincremental::double precision) AS rainincremental_total,
    -- Storm rainfall [storm_rain]
    max(stormrain) AS stormrain_max,
    -- Rainfall today [day_rain]
    max(dayrain) AS dayrain_max,
    -- Rainfall this month [month_rain]
    max(monthrain) AS monthrain_max,
    -- Rainfall this year [year_rain]
    max(yearrain) AS yearrain_max,
    -- Solar radiation [solar_watts]
    sum(solarwatts::double precision) AS solarwatts_sum,
    nullif(count(solarwatts), 0) AS solarwatts_count,
    min(solarwatts) AS solarwatts_min,
    max(solarwatts) AS solarwatts_max,
    -- Clear-sky solar radiation [potential_solar_watts]
    sum(potentialsolarwatts::double precision) AS potentialsolarwatts_sum,
    nullif(count(potentialsolarwatts), 0) AS potentialsolarwatts_count,
    min(potentialsolarwatts) AS potentialsolarwatts_min,
    max(potentialsolarwatts) AS potentialsolarwatts_max,
    -- Solar energy [solar_joules]
    sum(solarjoules::double precision) AS solarjoules_sum,
    nullif(count(solarjoules), 0) AS solarjoules_count,
    min(solarjoules) AS solarjoules_min,
    max(solarjoules) AS solarjoules_max,
    -- UV index [uv]
    sum(uv::double precision) AS uv_sum,
    nullif(count(uv), 0) AS uv_count,
    min(uv) AS uv_min,
    max(uv) AS uv_max,
    -- Solar radiation (sensor) [radiation]
    sum(radiation::double precision) AS radiation_sum,
    nullif(count(radiation), 0) AS radiation_count,
    min(radiation) AS radiation_min,
    max(radiation) AS radiation_max,
    -- Evapotranspiration today [day_et]
    max(dayet) AS dayet_max,
    -- Evapotranspiration this month [month_et]
    max(monthet) AS monthet_max,
    -- Evapotranspiration this year [year_et]
    max(yearet) AS yearet_max,
    -- Soil moisture 1 [soil_moisture1]
    sum(soilmoisture1::double precision) AS soilmoisture1_sum,
    nullif(count(soilmoisture1), 0) AS soilmoisture1_count,
    min(soilmoisture1) AS soilmoisture1_min,
    max(soilmoisture1) AS soilmoisture1_max,
    -- Soil moisture 2 [soil_moisture2]
    sum(soilmoisture2::double precision) AS soilmoisture2_sum,
    nullif(count(soilmoisture2), 0) AS soilmoisture2_count,
    min(soilmoisture2) AS soilmoisture2_min,
    max(soilmoisture2) AS soilmoisture2_max,
    -- Soil moisture 3 [soil_moisture3]
    sum(soilmoisture3::double precision) AS soilmoisture3_sum,
    nullif(count(soilmoisture3), 0) AS soilmoisture3_count,
    min(soilmoisture3) AS soilmoisture3_min,
    max(soilmoisture3) AS soilmoisture3_max,
    -- Soil moisture 4 [soil_moisture4]
    sum(soilmoisture4::double precision) AS soilmoisture4_sum,
    nullif(count(soilmoisture4), 0) AS soilmoisture4_count,
    min(soilmoisture4) AS soilmoisture4_min,
    max(soilmoisture4) AS soilmoisture4_max,
    -- Leaf wetness 1 [leaf_wetness1]
    sum(leafwetness1::double precision) AS leafwetness1_sum,
    nullif(count(leafwetness1), 0) AS leafwetness1_count,
    min(leafwetness1) AS leafwetness1_min,
    max(leafwetness1) AS leafwetness1_max,
    -- Leaf wetness 2 [leaf_wetness2]
    sum(leafwetness2::double precision) AS leafwetness2_sum,
    nullif(count(leafwetness2), 0) AS leafwetness2_count,
    min(leafwetness2) AS leafwetness2_min,
    max(leafwetness2) AS leafwetness2_max,
    -- Leaf wetness 3 [leaf_wetness3]
    sum(leafwetness3::double precision) AS leafwetness3_sum,
    nullif(count(leafwetness3), 0) AS leafwetness3_count,
    min(leafwetness3) AS leafwetness3_min,
    max(leafwetness3) AS leafwetness3_max,
    -- Leaf wetness 4 [leaf_wetness4]
    sum(leafwetness4::double precision) AS leafwetness4_sum,
    nullif(count(leafwetness4), 0) AS leafwetness4_count,
    min(leafwetness4) AS leafwetness4_min,
    max(leafwetness4) AS leafwetness4_max,
    -- Indoor alarm [inside_alarm]
    min(insidealarm) AS insidealarm_min,
    max(insidealarm) AS insidealarm_max,
    -- Rain alarm [rain_alarm]
    min(rainalarm) AS rainalarm_min,
    max(rainalarm) AS rainalarm_max,
    -- Outside alarm 1 [outside_alarm1]
    min(outsidealarm1) AS outsidealarm1_min,
    max(outsidealarm1) AS outsidealarm1_max,
    -- Outside alarm 2 [outside_alarm2]
    min(outsidealarm2) AS outsidealarm2_min,
    max(outsidealarm2) AS outsidealarm2_max,
    -- Extra alarm 1 [extra_alarm1]
    min(extraalarm1) AS extraalarm1_min,
    max(extraalarm1) AS extraalarm1_max,
    -- Extra alarm 2 [extra_alarm2]
    min(extraalarm2) AS extraalarm2_min,
    max(extraalarm2) AS extraalarm2_max,
    -- Extra alarm 3 [extra_alarm3]
    min(extraalarm3) AS extraalarm3_min,
    max(extraalarm3) AS extraalarm3_max,
    -- Extra alarm 4 [extra_alarm4]
    min(extraalarm4) AS extraalarm4_min,
    max(extraalarm4) AS extraalarm4_max,
    -- Extra alarm 5 [extra_alarm5]
    min(extraalarm5) AS extraalarm5_min,
    max(extraalarm5) AS extraalarm5_max,
    -- Extra alarm 6 [extra_alarm6]
    min(extraalarm6) AS extraalarm6_min,
    max(extraalarm6) AS extraalarm6_max,
    -- Extra alarm 7 [extra_alarm7]
    min(extraalarm7) AS extraalarm7_min,
    max(extraalarm7) AS extraalarm7_max,
    -- Extra alarm 8 [extra_alarm8]
    min(extraalarm8) AS extraalarm8_min,
    max(extraalarm8) AS extraalarm8_max,
    -- Soil/leaf alarm 1 [soil_leaf_alarm1]
    min(soilleafalarm1) AS soilleafalarm1_min,
    max(soilleafalarm1) AS soilleafalarm1_max,
    -- Soil/leaf alarm 2 [soil_leaf_alarm2]
    min(soilleafalarm2) AS soilleafalarm2_min,
    max(soilleafalarm2) AS soilleafalarm2_max,
    -- Soil/leaf alarm 3 [soil_leaf_alarm3]
    min(soilleafalarm3) AS soilleafalarm3_min,
    max(soilleafalarm3) AS soilleafalarm3_max,
    -- Soil/leaf alarm 4 [soil_leaf_alarm4]
    min(soilleafalarm4) AS soilleafalarm4_min,
    max(soilleafalarm4) AS soilleafalarm4_max,
    -- Transmitter battery [tx_battery_status]
    min(txbatterystatus) AS txbatterystatus_min,
    max(txbatterystatus) AS txbatterystatus_max,
    -- Console battery voltage [cons_battery_voltage]
    sum(consbatteryvoltage::double precision) AS consbatteryvoltage_sum,
    nullif(count(consbatteryvoltage), 0) AS consbatteryvoltage_count,
    min(consbatteryvoltage) AS consbatteryvoltage_min,
    max(consbatteryvoltage) AS consbatteryvoltage_max,
    -- Station battery voltage [station_battery_voltage]
    sum(stationbatteryvoltage::double precision) AS stationbatteryvoltage_sum,
    nullif(count(stationbatteryvoltage), 0) AS stationbatteryvoltage_count,
    min(stationbatteryvoltage) AS stationbatteryvoltage_min,
    max(stationbatteryvoltage) AS stationbatteryvoltage_max,
    -- Snow sensor distance [snow_distance]
    sum(snowdistance::double precision) AS snowdistance_sum,
    nullif(count(snowdistance), 0) AS snowdistance_count,
    min(snowdistance) AS snowdistance_min,
    max(snowdistance) AS snowdistance_max,
    -- Snow depth [snow_depth]
    sum(snowdepth::double precision) AS snowdepth_sum,
    nullif(count(snowdepth), 0) AS snowdepth_count,
    min(snowdepth) AS snowdepth_min,
    max(snowdepth) AS snowdepth_max,
    -- Extra value 1 [extra_float1]
    sum(extrafloat1::double precision) AS extrafloat1_sum,
    nullif(count(extrafloat1), 0) AS extrafloat1_count,
    min(extrafloat1) AS extrafloat1_min,
    max(extrafloat1) AS extrafloat1_max,
    -- Extra value 2 [extra_float2]
    sum(extrafloat2::double precision) AS extrafloat2_sum,
    nullif(count(extrafloat2), 0) AS extrafloat2_count,
    min(extrafloat2) AS extrafloat2_min,
    max(extrafloat2) AS extrafloat2_max,
    -- Extra value 3 [extra_float3]
    sum(extrafloat3::double precision) AS extrafloat3_sum,
    nullif(count(extrafloat3), 0) AS extrafloat3_count,
    min(extrafloat3) AS extrafloat3_min,
    max(extrafloat3) AS extrafloat3_max,
    -- Extra value 4 [extra_float4]
    sum(extrafloat4::double precision) AS extrafloat4_sum,
    nullif(count(extrafloat4), 0) AS extrafloat4_count,
    min(extrafloat4) AS extrafloat4_min,
    max(extrafloat4) AS extrafloat4_max,
    -- Extra value 5 [extra_float5]
    sum(extrafloat5::double precision) AS extrafloat5_sum,
    nullif(count(extrafloat5), 0) AS extrafloat5_count,
    min(extrafloat5) AS extrafloat5_min,
    max(extrafloat5) AS extrafloat5_max,
    -- Extra value 6 [extra_float6]
    sum(extrafloat6::double precision) AS extrafloat6_sum,
    nullif(count(extrafloat6), 0) AS extrafloat6_count,
    min(extrafloat6) AS extrafloat6_min,
    max(extrafloat6) AS extrafloat6_max,
    -- Extra value 7 [extra_float7]
    sum(extrafloat7::double precision) AS extrafloat7_sum,
    nullif(count(extrafloat7), 0) AS extrafloat7_count,
    min(extrafloat7) AS extrafloat7_min,
    max(extrafloat7) AS extrafloat7_max,
    -- Extra value 8 [extra_float8]
    sum(extrafloat8::double precision) AS extrafloat8_sum,
    nullif(count(extrafloat8), 0) AS extrafloat8_count,
    min(extrafloat8) AS extrafloat8_min,
    max(extrafloat8) AS extrafloat8_max,
    -- Extra value 9 [extra_float9]
    sum(extrafloat9::double precision) AS extrafloat9_sum,
    nullif(count(extrafloat9), 0) AS extrafloat9_count,
    min(extrafloat9) AS extrafloat9_min,
    max(extrafloat9) AS extrafloat9_max,
    -- Extra value 10 [extra_float10]
    sum(extrafloat10::double precision) AS extrafloat10_sum,
    nullif(count(extrafloat10), 0) AS extrafloat10_count,
    min(extrafloat10) AS extrafloat10_min,
    max(extrafloat10) AS extrafloat10_max,
    -- Temperature sensor 1 [temp1]
    sum(temp1::double precision) AS temp1_sum,
    nullif(count(temp1), 0) AS temp1_count,
    min(temp1) AS temp1_min,
    max(temp1) AS temp1_max,
    -- Temperature sensor 2 [temp2]
    sum(temp2::double precision) AS temp2_sum,
    nullif(count(temp2), 0) AS temp2_count,
    min(temp2) AS temp2_min,
    max(temp2) AS temp2_max,
    -- Temperature sensor 3 [temp3]
    sum(temp3::double precision) AS temp3_sum,
    nullif(count(temp3), 0) AS temp3_count,
    min(temp3) AS temp3_min,
    max(temp3) AS temp3_max,
    -- Temperature sensor 4 [temp4]
    sum(temp4::double precision) AS temp4_sum,
    nullif(count(temp4), 0) AS temp4_count,
    min(temp4) AS temp4_min,
    max(temp4) AS temp4_max,
    -- Temperature sensor 5 [temp5]
    sum(temp5::double precision) AS temp5_sum,
    nullif(count(temp5), 0) AS temp5_count,
    min(temp5) AS temp5_min,
    max(temp5) AS temp5_max,
    -- Temperature sensor 6 [temp6]
    sum(temp6::double precision) AS temp6_sum,
    nullif(count(temp6), 0) AS temp6_count,
    min(temp6) AS temp6_min,
    max(temp6) AS temp6_max,
    -- Temperature sensor 7 [temp7]
    sum(temp7::double precision) AS temp7_sum,
    nullif(count(temp7), 0) AS temp7_count,
    min(temp7) AS temp7_min,
    max(temp7) AS temp7_max,
    -- Temperature sensor 8 [temp8]
    sum(temp8::double precision) AS temp8_sum,
    nullif(count(temp8), 0) AS temp8_count,
    min(temp8) AS temp8_min,
    max(temp8) AS temp8_max,
    -- Temperature sensor 9 [temp9]
    sum(temp9::double precision) AS temp9_sum,
    nullif(count(temp9), 0) AS temp9_count,
    min(temp9) AS temp9_min,
    max(temp9) AS temp9_max,
    -- Temperature sensor 10 [temp10]
    sum(temp10::double precision) AS temp10_sum,
    nullif(count(temp10), 0) AS temp10_count,
    min(temp10) AS temp10_min,
    max(temp10) AS temp10_max,
    -- Soil temperature 5 [soil_temp5]
    sum(soiltemp5::double precision) AS soiltemp5_sum,
    nullif(count(soiltemp5), 0) AS soiltemp5_count,
    min(soiltemp5) AS soiltemp5_min,
    max(soiltemp5) AS soiltemp5_max,
    -- Soil temperature 6 [soil_temp6]
    sum(soiltemp6::double precision) AS soiltemp6_sum,
    nullif(count(soiltemp6), 0) AS soiltemp6_count,
    min(soiltemp6) AS soiltemp6_min,
    max(soiltemp6) AS soiltemp6_max,
    -- Soil temperature 7 [soil_temp7]
    sum(soiltemp7::double precision) AS soiltemp7_sum,
    nullif(count(soiltemp7), 0) AS soiltemp7_count,
    min(soiltemp7) AS soiltemp7_min,
    max(soiltemp7) AS soiltemp7_max,
    -- Soil temperature 8 [soil_temp8]
    sum(soiltemp8::double precision) AS soiltemp8_sum,
    nullif(count(soiltemp8), 0) AS soiltemp8_count,
    min(soiltemp8) AS soiltemp8_min,
    max(soiltemp8) AS soiltemp8_max,
    -- Soil temperature 9 [soil_temp9]
    sum(soiltemp9::double precision) AS soiltemp9_sum,
    nullif(count(soiltemp9), 0) AS soiltemp9_count,
    min(soiltemp9) AS soiltemp9_min,
    max(soiltemp9) AS soiltemp9_max,
    -- Soil temperature 10 [soil_temp10]
    sum(soiltemp10::double precision) AS soiltemp10_sum,
    nullif(count(soiltemp10), 0) AS soiltemp10_count,
    min(soiltemp10) AS soiltemp10_min,
    max(soiltemp10) AS soiltemp10_max,
    -- Humidity sensor 1 [humidity1]
    sum(humidity1::double precision) AS humidity1_sum,
    nullif(count(humidity1), 0) AS humidity1_count,
    min(humidity1) AS humidity1_min,
    max(humidity1) AS humidity1_max,
    -- Humidity sensor 2 [humidity2]
    sum(humidity2::double precision) AS humidity2_sum,
    nullif(count(humidity2), 0) AS humidity2_count,
    min(humidity2) AS humidity2_min,
    max(humidity2) AS humidity2_max,
    -- Humidity sensor 3 [humidity3]
    sum(humidity3::double precision) AS humidity3_sum,
    nullif(count(humidity3), 0) AS humidity3_count,
    min(humidity3) AS humidity3_min,
    max(humidity3) AS humidity3_max,
    -- Humidity sensor 4 [humidity4]
    sum(humidity4::double precision) AS humidity4_sum,
    nullif(count(humidity4), 0) AS humidity4_count,
    min(humidity4) AS humidity4_min,
    max(humidity4) AS humidity4_max,
    -- Humidity sensor 5 [humidity5]
    sum(humidity5::double precision) AS humidity5_sum,
    nullif(count(humidity5), 0) AS humidity5_count,
    min(humidity5) AS humidity5_min,
    max(humidity5) AS humidity5_max,
    -- Humidity sensor 6 [humidity6]
    sum(humidity6::double precision) AS humidity6_sum,
    nullif(count(humidity6), 0) AS humidity6_count,
    min(humidity6) AS humidity6_min,
    max(humidity6) AS humidity6_max,
    -- Humidity sensor 7 [humidity7]
    sum(humidity7::double precision) AS humidity7_sum,
    nullif(count(humidity7), 0) AS humidity7_count,
    min(humidity7) AS humidity7_min,
    max(humidity7) AS humidity7_max,
    -- Humidity sensor 8 [humidity8]
    sum(humidity8::double precision) AS humidity8_sum,
    nullif(count(humidity8), 0) AS humidity8_count,
    min(humidity8) AS humidity8_min,
    max(humidity8) AS humidity8_max,
    -- Humidity sensor 9 [humidity9]
    sum(humidity9::double precision) AS humidity9_sum,
    nullif(count(humidity9), 0) AS humidity9_count,
    min(humidity9) AS humidity9_min,
    max(humidity9) AS humidity9_max,
    -- Humidity sensor 10 [humidity10]
    sum(humidity10::double precision) AS humidity10_sum,
    nullif(count(humidity10), 0) AS humidity10_count,
    min(humidity10) AS humidity10_min,
    max(humidity10) AS humidity10_max,
    -- Soil humidity 1 [soil_hum1]
    sum(soilhum1::double precision) AS soilhum1_sum,
    nullif(count(soilhum1), 0) AS soilhum1_count,
    min(soilhum1) AS soilhum1_min,
    max(soilhum1) AS soilhum1_max,
    -- Soil humidity 2 [soil_hum2]
    sum(soilhum2::double precision) AS soilhum2_sum,
    nullif(count(soilhum2), 0) AS soilhum2_count,
    min(soilhum2) AS soilhum2_min,
    max(soilhum2) AS soilhum2_max,
    -- Soil humidity 3 [soil_hum3]
    sum(soilhum3::double precision) AS soilhum3_sum,
    nullif(count(soilhum3), 0) AS soilhum3_count,
    min(soilhum3) AS soilhum3_min,
    max(soilhum3) AS soilhum3_max,
    -- Soil humidity 4 [soil_hum4]
    sum(soilhum4::double precision) AS soilhum4_sum,
    nullif(count(soilhum4), 0) AS soilhum4_count,
    min(soilhum4) AS soilhum4_min,
    max(soilhum4) AS soilhum4_max,
    -- Soil humidity 5 [soil_hum5]
    sum(soilhum5::double precision) AS soilhum5_sum,
    nullif(count(soilhum5), 0) AS soilhum5_count,
    min(soilhum5) AS soilhum5_min,
    max(soilhum5) AS soilhum5_max,
    -- Soil humidity 6 [soil_hum6]
    sum(soilhum6::double precision) AS soilhum6_sum,
    nullif(count(soilhum6), 0) AS soilhum6_count,
    min(soilhum6) AS soilhum6_min,
    max(soilhum6) AS soilhum6_max,
    -- Soil humidity 7 [soil_hum7]
    sum(soilhum7::double precision) AS soilhum7_sum,
    nullif(count(soilhum7), 0) AS soilhum7_count,
    min(soilhum7) AS soilhum7_min,
    max(soilhum7) AS soilhum7_max,
    -- Soil humidity 8 [soil_hum8]
    sum(soilhum8::double precision) AS soilhum8_sum,
    nullif(count(soilhum8), 0) AS soilhum8_count,
    min(soilhum8) AS soilhum8_min,
    max(soilhum8) AS soilhum8_max,
    -- Soil humidity 9 [soil_hum9]
    sum(soilhum9::double precision) AS soilhum9_sum,
    nullif(count(soilhum9), 0) AS soilhum9_count,
    min(soilhum9) AS soilhum9_min,
    max(soilhum9) AS soilhum9_max,
    -- Soil humidity 10 [soil_hum10]
    sum(soilhum10::double precision) AS soilhum10_sum,
    nullif(count(soilhum10), 0) AS soilhum10_count,
    min(soilhum10) AS soilhum10_min,
    max(soilhum10) AS soilhum10_max,
    -- Leaf wetness 5 [leaf_wetness5]
    sum(leafwetness5::double precision) AS leafwetness5_sum,
    nullif(count(leafwetness5), 0) AS leafwetness5_count,
    min(leafwetness5) AS leafwetness5_min,
    max(leafwetness5) AS leafwetness5_max,
    -- Leaf wetness 6 [leaf_wetness6]
    sum(leafwetness6::double precision) AS leafwetness6_sum,
    nullif(count(leafwetness6), 0) AS leafwetness6_count,
    min(leafwetness6) AS leafwetness6_min,
    max(leafwetness6) AS leafwetness6_max,
    -- Leaf wetness 7 [leaf_wetness7]
    sum(leafwetness7::double precision) AS leafwetness7_sum,
    nullif(count(leafwetness7), 0) AS leafwetness7_count,
    min(leafwetness7) AS leafwetness7_min,
    max(leafwetness7) AS leafwetness7_max,
    -- Leaf wetness 8 [leaf_wetness8]
    sum(leafwetness8::double precision) AS leafwetness8_sum,
    nullif(count(leafwetness8), 0) AS leafwetness8_count,
    min(leafwetness8) AS leafwetness8_min,
    max(leafwetness8) AS leafwetness8_max,
    -- Soil tension 1 [soil_tens1]
    sum(soiltens1::double precision) AS soiltens1_sum,
    nullif(count(soiltens1), 0) AS soiltens1_count,
    min(soiltens1) AS soiltens1_min,
    max(soiltens1) AS soiltens1_max,
    -- Soil tension 2 [soil_tens2]
    sum(soiltens2::double precision) AS soiltens2_sum,
    nullif(count(soiltens2), 0) AS soiltens2_count,
    min(soiltens2) AS soiltens2_min,
    max(soiltens2) AS soiltens2_max,
    -- Soil tension 3 [soil_tens3]
    sum(soiltens3::double precision) AS soiltens3_sum,
    nullif(count(soiltens3), 0) AS soiltens3_count,
    min(soiltens3) AS soiltens3_min,
    max(soiltens3) AS soiltens3_max,
    -- Soil tension 4 [soil_tens4]
    sum(soiltens4::double precision) AS soiltens4_sum,
    nullif(count(soiltens4), 0) AS soiltens4_count,
    min(soiltens4) AS soiltens4_min,
    max(soiltens4) AS soiltens4_max,
    -- Growing degree days [gdd]
    max(gdd) AS gdd_max,
    -- Reference evapotranspiration (short crop) [etos]
    sum(etos::double precision) AS etos_sum,
    nullif(count(etos), 0) AS etos_count,
    min(etos) AS etos_min,
    max(etos) AS etos_max,
    -- Reference evapotranspiration (tall crop) [etrs]
    sum(etrs::double precision) AS etrs_sum,
    nullif(count(etrs), 0) AS etrs_count,
    min(etrs) AS etrs_min,
    max(etrs) AS etrs_max,
    -- Leak detector 1 [leak1]
    min(leak1) AS leak1_min,
    max(leak1) AS leak1_max,
    -- Leak detector 2 [leak2]
    min(leak2) AS leak2_min,
    max(leak2) AS leak2_max,
    -- Leak detector 3 [leak3]
    min(leak3) AS leak3_min,
    max(leak3) AS leak3_max,
    -- Leak detector 4 [leak4]
    min(leak4) AS leak4_min,
    max(leak4) AS leak4_max,
    -- Outdoor sensor battery [batt_out]
    min(battout) AS battout_min,
    max(battout) AS battout_max,
    -- Indoor sensor battery [batt_in]
    min(battin) AS battin_min,
    max(battin) AS battin_max,
    -- Battery 1 [batt1]
    min(batt1) AS batt1_min,
    max(batt1) AS batt1_max,
    -- Battery 2 [batt2]
    min(batt2) AS batt2_min,
    max(batt2) AS batt2_max,
    -- Battery 3 [batt3]
    min(batt3) AS batt3_min,
    max(batt3) AS batt3_max,
    -- Battery 4 [batt4]
    min(batt4) AS batt4_min,
    max(batt4) AS batt4_max,
    -- Battery 5 [batt5]
    min(batt5) AS batt5_min,
    max(batt5) AS batt5_max,
    -- Battery 6 [batt6]
    min(batt6) AS batt6_min,
    max(batt6) AS batt6_max,
    -- Battery 7 [batt7]
    min(batt7) AS batt7_min,
    max(batt7) AS batt7_max,
    -- Battery 8 [batt8]
    min(batt8) AS batt8_min,
    max(batt8) AS batt8_max,
    -- Battery 9 [batt9]
    min(batt9) AS batt9_min,
    max(batt9) AS batt9_max,
    -- Battery 10 [batt10]
    min(batt10) AS batt10_min,
    max(batt10) AS batt10_max,
    -- Particulate sensor battery [batt_25]
    min(batt_25) AS batt_25_min,
    max(batt_25) AS batt_25_max,
    -- Lightning sensor battery [batt_lightning]
    min(batt_lightning) AS batt_lightning_min,
    max(batt_lightning) AS batt_lightning_max,
    -- Leak detector 1 battery [bat_leak1]
    min(batleak1) AS batleak1_min,
    max(batleak1) AS batleak1_max,
    -- Leak detector 2 battery [bat_leak2]
    min(batleak2) AS batleak2_min,
    max(batleak2) AS batleak2_max,
    -- Leak detector 3 battery [bat_leak3]
    min(batleak3) AS batleak3_min,
    max(batleak3) AS batleak3_max,
    -- Leak detector 4 battery [bat_leak4]
    min(batleak4) AS batleak4_min,
    max(batleak4) AS batleak4_max,
    -- Soil moisture sensor 1 battery [batt_sm1]
    min(battsm1) AS battsm1_min,
    max(battsm1) AS battsm1_max,
    -- Soil moisture sensor 2 battery [batt_sm2]
    min(battsm2) AS battsm2_min,
    max(battsm2) AS battsm2_max,
    -- Soil moisture sensor 3 battery [batt_sm3]
    min(battsm3) AS battsm3_min,
    max(battsm3) AS battsm3_max,
    -- Soil moisture sensor 4 battery [batt_sm4]
    min(battsm4) AS battsm4_min,
    max(battsm4) AS battsm4_max,
    -- CO2 sensor battery [batt_co2]
    min(batt_co2) AS batt_co2_min,
    max(batt_co2) AS batt_co2_max,
    -- Cellular gateway battery [batt_cellgateway]
    min(batt_cellgateway) AS batt_cellgateway_min,
    max(batt_cellgateway) AS batt_cellgateway_max,
    -- Pressure (relative) [barom_rel_in]
    sum(baromrelin::double precision) AS baromrelin_sum,
    nullif(count(baromrelin), 0) AS baromrelin_count,
    min(baromrelin) AS baromrelin_min,
    max(baromrelin) AS baromrelin_max,
    -- Pressure (absolute) [barom_abs_in]
    sum(baromabsin::double precision) AS baromabsin_sum,
    nullif(count(baromabsin), 0) AS baromabsin_count,
    min(baromabsin) AS baromabsin_min,
    max(baromabsin) AS baromabsin_max,
    -- Relay 1 [relay1]
    min(relay1) AS relay1_min,
    max(relay1) AS relay1_max,
    -- Relay 2 [relay2]
    min(relay2) AS relay2_min,
    max(relay2) AS relay2_max,
    -- Relay 3 [relay3]
    min(relay3) AS relay3_min,
    max(relay3) AS relay3_max,
    -- Relay 4 [relay4]
    min(relay4) AS relay4_min,
    max(relay4) AS relay4_max,
    -- Relay 5 [relay5]
    min(relay5) AS relay5_min,
    max(relay5) AS relay5_max,
    -- Relay 6 [relay6]
    min(relay6) AS relay6_min,
    max(relay6) AS relay6_max,
    -- Relay 7 [relay7]
    min(relay7) AS relay7_min,
    max(relay7) AS relay7_max,
    -- Relay 8 [relay8]
    min(relay8) AS relay8_min,
    max(relay8) AS relay8_max,
    -- Relay 9 [relay9]
    min(relay9) AS relay9_min,
    max(relay9) AS relay9_max,
    -- Relay 10 [relay10]
    min(relay10) AS relay10_min,
    max(relay10) AS relay10_max,
    -- PM2.5 [pm25]
    sum(pm25::double precision) AS pm25_sum,
    nullif(count(pm25), 0) AS pm25_count,
    min(pm25) AS pm25_min,
    max(pm25) AS pm25_max,
    -- PM2.5 (24-hour average) [pm25_24h]
    sum(pm25_24h::double precision) AS pm25_24h_sum,
    nullif(count(pm25_24h), 0) AS pm25_24h_count,
    min(pm25_24h) AS pm25_24h_min,
    max(pm25_24h) AS pm25_24h_max,
    -- PM2.5 indoors [pm25_in]
    sum(pm25_in::double precision) AS pm25_in_sum,
    nullif(count(pm25_in), 0) AS pm25_in_count,
    min(pm25_in) AS pm25_in_min,
    max(pm25_in) AS pm25_in_max,
    -- PM2.5 indoors (24-hour average) [pm25_in_24h]
    sum(pm25_in_24h::double precision) AS pm25_in_24h_sum,
    nullif(count(pm25_in_24h), 0) AS pm25_in_24h_count,
    min(pm25_in_24h) AS pm25_in_24h_min,
    max(pm25_in_24h) AS pm25_in_24h_max,
    -- PM2.5 indoors (AQIN sensor) [pm25_in_aqin]
    sum(pm25_in_aqin::double precision) AS pm25_in_aqin_sum,
    nullif(count(pm25_in_aqin), 0) AS pm25_in_aqin_count,
    min(pm25_in_aqin) AS pm25_in_aqin_min,
    max(pm25_in_aqin) AS pm25_in_aqin_max,
    -- PM2.5 indoors, 24-hour average (AQIN sensor) [pm25_in_24h_aqin]
    sum(pm25_in_24h_aqin::double precision) AS pm25_in_24h_aqin_sum,
    nullif(count(pm25_in_24h_aqin), 0) AS pm25_in_24h_aqin_count,
    min(pm25_in_24h_aqin) AS pm25_in_24h_aqin_min,
    max(pm25_in_24h_aqin) AS pm25_in_24h_aqin_max,
    -- PM10 indoors (AQIN sensor) [pm10_in_aqin]
    sum(pm10_in_aqin::double precision) AS pm10_in_aqin_sum,
    nullif(count(pm10_in_aqin), 0) AS pm10_in_aqin_count,
    min(pm10_in_aqin) AS pm10_in_aqin_min,
    max(pm10_in_aqin) AS pm10_in_aqin_max,
    -- PM10 indoors, 24-hour average (AQIN sensor) [pm10_in_24h_aqin]
    sum(pm10_in_24h_aqin::double precision) AS pm10_in_24h_aqin_sum,
    nullif(count(pm10_in_24h_aqin), 0) AS pm10_in_24h_aqin_count,
    min(pm10_in_24h_aqin) AS pm10_in_24h_aqin_min,
    max(pm10_in_24h_aqin) AS pm10_in_24h_aqin_max,
    -- CO2 [co2]
    sum(co2::double precision) AS co2_sum,
    nullif(count(co2), 0) AS co2_count,
    min(co2) AS co2_min,
    max(co2) AS co2_max,
    -- CO2 indoors (AQIN sensor) [co2_in_aqin]
    sum(co2_in_aqin::double precision) AS co2_in_aqin_sum,
    nullif(count(co2_in_aqin), 0) AS co2_in_aqin_count,
    min(co2_in_aqin) AS co2_in_aqin_min,
    max(co2_in_aqin) AS co2_in_aqin_max,
    -- CO2 indoors, 24-hour average (AQIN sensor) [co2_in_24h_aqin]
    sum(co2_in_24h_aqin::double precision) AS co2_in_24h_aqin_sum,
    nullif(count(co2_in_24h_aqin), 0) AS co2_in_24h_aqin_count,
    min(co2_in_24h_aqin) AS co2_in_24h_aqin_min,
    max(co2_in_24h_aqin) AS co2_in_24h_aqin_max,
    -- Indoor temperature (AQIN sensor) [pm_in_temp_aqin]
    sum(pm_in_temp_aqin::double precision) AS pm_in_temp_aqin_sum,
    nullif(count(pm_in_temp_aqin), 0) AS pm_in_temp_aqin_count,
    min(pm_in_temp_aqin) AS pm_in_temp_aqin_min,
    max(pm_in_temp_aqin) AS pm_in_temp_aqin_max,
    -- Indoor humidity (AQIN sensor) [pm_in_humidity_aqin]
    sum(pm_in_humidity_aqin::double precision) AS pm_in_humidity_aqin_sum,
    nullif(count(pm_in_humidity_aqin), 0) AS pm_in_humidity_aqin_count,
    min(pm_in_humidity_aqin) AS pm_in_humidity_aqin_min,
    max(pm_in_humidity_aqin) AS pm_in_humidity_aqin_max,
    -- Air quality index, PM2.5 (AQIN sensor) [aqi_pm25_aqin]
    sum(aqi_pm25_aqin::double precision) AS aqi_pm25_aqin_sum,
    nullif(count(aqi_pm25_aqin), 0) AS aqi_pm25_aqin_count,
    min(aqi_pm25_aqin) AS aqi_pm25_aqin_min,
    max(aqi_pm25_aqin) AS aqi_pm25_aqin_max,
    -- Air quality index, PM2.5 24-hour (AQIN sensor) [aqi_pm25_24h_aqin]
    sum(aqi_pm25_24h_aqin::double precision) AS aqi_pm25_24h_aqin_sum,
    nullif(count(aqi_pm25_24h_aqin), 0) AS aqi_pm25_24h_aqin_count,
    min(aqi_pm25_24h_aqin) AS aqi_pm25_24h_aqin_min,
    max(aqi_pm25_24h_aqin) AS aqi_pm25_24h_aqin_max,
    -- Air quality index, PM10 (AQIN sensor) [aqi_pm10_aqin]
    sum(aqi_pm10_aqin::double precision) AS aqi_pm10_aqin_sum,
    nullif(count(aqi_pm10_aqin), 0) AS aqi_pm10_aqin_count,
    min(aqi_pm10_aqin) AS aqi_pm10_aqin_min,
    max(aqi_pm10_aqin) AS aqi_pm10_aqin_max,
    -- Air quality index, PM10 24-hour (AQIN sensor) [aqi_pm10_24h_aqin]
    sum(aqi_pm10_24h_aqin::double precision) AS aqi_pm10_24h_aqin_sum,
    nullif(count(aqi_pm10_24h_aqin), 0) AS aqi_pm10_24h_aqin_count,
    min(aqi_pm10_24h_aqin) AS aqi_pm10_24h_aqin_min,
    max(aqi_pm10_24h_aqin) AS aqi_pm10_24h_aqin_max,
    -- Air quality index, PM2.5 indoors [aqi_pm25_in]
    sum(aqi_pm25_in::double precision) AS aqi_pm25_in_sum,
    nullif(count(aqi_pm25_in), 0) AS aqi_pm25_in_count,
    min(aqi_pm25_in) AS aqi_pm25_in_min,
    max(aqi_pm25_in) AS aqi_pm25_in_max,
    -- Air quality index, PM2.5 indoors 24-hour [aqi_pm25_in_24h]
    sum(aqi_pm25_in_24h::double precision) AS aqi_pm25_in_24h_sum,
    nullif(count(aqi_pm25_in_24h), 0) AS aqi_pm25_in_24h_count,
    min(aqi_pm25_in_24h) AS aqi_pm25_in_24h_min,
    max(aqi_pm25_in_24h) AS aqi_pm25_in_24h_max,
    -- Lightning strikes today [lightning_day]
    max(lightning_day) AS lightning_day_max,
    -- Lightning strikes this hour [lightning_hour]
    max(lightning_hour) AS lightning_hour_max,
    -- Distance to last lightning strike [lightning_distance]
    sum(lightning_distance::double precision) AS lightning_distance_sum,
    nullif(count(lightning_distance), 0) AS lightning_distance_count,
    min(lightning_distance) AS lightning_distance_min,
    max(lightning_distance) AS lightning_distance_max
FROM observations
GROUP BY 1, 2
) q;
\timing off
RESET max_parallel_workers_per_gather;

-- Same aggregation, materialised into a plain table with relaxed durability. This is the
-- closest analogue to what refresh_continuous_aggregate actually does, and the difference
-- between it and the refresh is the refresh machinery rather than the arithmetic.
DROP TABLE IF EXISTS mw_measure.plain_1m;
SET synchronous_commit = off;
\timing on
CREATE TABLE mw_measure.plain_1m AS 
SELECT
    time_bucket(INTERVAL '1 minute', time) AS bucket,
    station_id,
    -- Pressure [barometer]
    sum(barometer::double precision) AS barometer_sum,
    nullif(count(barometer), 0) AS barometer_count,
    min(barometer) AS barometer_min,
    max(barometer) AS barometer_max,
    -- Indoor temperature [in_temp]
    sum(intemp::double precision) AS intemp_sum,
    nullif(count(intemp), 0) AS intemp_count,
    min(intemp) AS intemp_min,
    max(intemp) AS intemp_max,
    -- Indoor humidity [in_humidity]
    sum(inhumidity::double precision) AS inhumidity_sum,
    nullif(count(inhumidity), 0) AS inhumidity_count,
    min(inhumidity) AS inhumidity_min,
    max(inhumidity) AS inhumidity_max,
    -- Temperature [out_temp]
    sum(outtemp::double precision) AS outtemp_sum,
    nullif(count(outtemp), 0) AS outtemp_count,
    min(outtemp) AS outtemp_min,
    max(outtemp) AS outtemp_max,
    -- Humidity [out_humidity]
    sum(outhumidity::double precision) AS outhumidity_sum,
    nullif(count(outhumidity), 0) AS outhumidity_count,
    min(outhumidity) AS outhumidity_min,
    max(outhumidity) AS outhumidity_max,
    -- Wind speed [wind_speed]
    sum(windspeed::double precision) AS windspeed_sum,
    nullif(count(windspeed), 0) AS windspeed_count,
    max(windspeed) AS windspeed_max,
    -- Wind speed (10-minute average) [wind_speed10]
    sum(windspeed10::double precision) AS windspeed10_sum,
    nullif(count(windspeed10), 0) AS windspeed10_count,
    max(windspeed10) AS windspeed10_max,
    -- Wind direction [wind_dir]
    sum(sind(winddir::double precision)) AS winddir_sin_sum,
    sum(cosd(winddir::double precision)) AS winddir_cos_sum,
    nullif(count(winddir), 0) AS winddir_dir_count,
    -- Wind chill [wind_chill]
    sum(windchill::double precision) AS windchill_sum,
    nullif(count(windchill), 0) AS windchill_count,
    min(windchill) AS windchill_min,
    -- Heat index [heat_index]
    sum(heatindex::double precision) AS heatindex_sum,
    nullif(count(heatindex), 0) AS heatindex_count,
    max(heatindex) AS heatindex_max,
    -- Extra temperature 1 [extra_temp1]
    sum(extratemp1::double precision) AS extratemp1_sum,
    nullif(count(extratemp1), 0) AS extratemp1_count,
    min(extratemp1) AS extratemp1_min,
    max(extratemp1) AS extratemp1_max,
    -- Extra temperature 2 [extra_temp2]
    sum(extratemp2::double precision) AS extratemp2_sum,
    nullif(count(extratemp2), 0) AS extratemp2_count,
    min(extratemp2) AS extratemp2_min,
    max(extratemp2) AS extratemp2_max,
    -- Extra temperature 3 [extra_temp3]
    sum(extratemp3::double precision) AS extratemp3_sum,
    nullif(count(extratemp3), 0) AS extratemp3_count,
    min(extratemp3) AS extratemp3_min,
    max(extratemp3) AS extratemp3_max,
    -- Extra temperature 4 [extra_temp4]
    sum(extratemp4::double precision) AS extratemp4_sum,
    nullif(count(extratemp4), 0) AS extratemp4_count,
    min(extratemp4) AS extratemp4_min,
    max(extratemp4) AS extratemp4_max,
    -- Extra temperature 5 [extra_temp5]
    sum(extratemp5::double precision) AS extratemp5_sum,
    nullif(count(extratemp5), 0) AS extratemp5_count,
    min(extratemp5) AS extratemp5_min,
    max(extratemp5) AS extratemp5_max,
    -- Extra temperature 6 [extra_temp6]
    sum(extratemp6::double precision) AS extratemp6_sum,
    nullif(count(extratemp6), 0) AS extratemp6_count,
    min(extratemp6) AS extratemp6_min,
    max(extratemp6) AS extratemp6_max,
    -- Extra temperature 7 [extra_temp7]
    sum(extratemp7::double precision) AS extratemp7_sum,
    nullif(count(extratemp7), 0) AS extratemp7_count,
    min(extratemp7) AS extratemp7_min,
    max(extratemp7) AS extratemp7_max,
    -- Soil temperature 1 [soil_temp1]
    sum(soiltemp1::double precision) AS soiltemp1_sum,
    nullif(count(soiltemp1), 0) AS soiltemp1_count,
    min(soiltemp1) AS soiltemp1_min,
    max(soiltemp1) AS soiltemp1_max,
    -- Soil temperature 2 [soil_temp2]
    sum(soiltemp2::double precision) AS soiltemp2_sum,
    nullif(count(soiltemp2), 0) AS soiltemp2_count,
    min(soiltemp2) AS soiltemp2_min,
    max(soiltemp2) AS soiltemp2_max,
    -- Soil temperature 3 [soil_temp3]
    sum(soiltemp3::double precision) AS soiltemp3_sum,
    nullif(count(soiltemp3), 0) AS soiltemp3_count,
    min(soiltemp3) AS soiltemp3_min,
    max(soiltemp3) AS soiltemp3_max,
    -- Soil temperature 4 [soil_temp4]
    sum(soiltemp4::double precision) AS soiltemp4_sum,
    nullif(count(soiltemp4), 0) AS soiltemp4_count,
    min(soiltemp4) AS soiltemp4_min,
    max(soiltemp4) AS soiltemp4_max,
    -- Leaf temperature 1 [leaf_temp1]
    sum(leaftemp1::double precision) AS leaftemp1_sum,
    nullif(count(leaftemp1), 0) AS leaftemp1_count,
    min(leaftemp1) AS leaftemp1_min,
    max(leaftemp1) AS leaftemp1_max,
    -- Leaf temperature 2 [leaf_temp2]
    sum(leaftemp2::double precision) AS leaftemp2_sum,
    nullif(count(leaftemp2), 0) AS leaftemp2_count,
    min(leaftemp2) AS leaftemp2_min,
    max(leaftemp2) AS leaftemp2_max,
    -- Leaf temperature 3 [leaf_temp3]
    sum(leaftemp3::double precision) AS leaftemp3_sum,
    nullif(count(leaftemp3), 0) AS leaftemp3_count,
    min(leaftemp3) AS leaftemp3_min,
    max(leaftemp3) AS leaftemp3_max,
    -- Leaf temperature 4 [leaf_temp4]
    sum(leaftemp4::double precision) AS leaftemp4_sum,
    nullif(count(leaftemp4), 0) AS leaftemp4_count,
    min(leaftemp4) AS leaftemp4_min,
    max(leaftemp4) AS leaftemp4_max,
    -- Extra humidity 1 [extra_humidity1]
    sum(extrahumidity1::double precision) AS extrahumidity1_sum,
    nullif(count(extrahumidity1), 0) AS extrahumidity1_count,
    min(extrahumidity1) AS extrahumidity1_min,
    max(extrahumidity1) AS extrahumidity1_max,
    -- Extra humidity 2 [extra_humidity2]
    sum(extrahumidity2::double precision) AS extrahumidity2_sum,
    nullif(count(extrahumidity2), 0) AS extrahumidity2_count,
    min(extrahumidity2) AS extrahumidity2_min,
    max(extrahumidity2) AS extrahumidity2_max,
    -- Extra humidity 3 [extra_humidity3]
    sum(extrahumidity3::double precision) AS extrahumidity3_sum,
    nullif(count(extrahumidity3), 0) AS extrahumidity3_count,
    min(extrahumidity3) AS extrahumidity3_min,
    max(extrahumidity3) AS extrahumidity3_max,
    -- Extra humidity 4 [extra_humidity4]
    sum(extrahumidity4::double precision) AS extrahumidity4_sum,
    nullif(count(extrahumidity4), 0) AS extrahumidity4_count,
    min(extrahumidity4) AS extrahumidity4_min,
    max(extrahumidity4) AS extrahumidity4_max,
    -- Extra humidity 5 [extra_humidity5]
    sum(extrahumidity5::double precision) AS extrahumidity5_sum,
    nullif(count(extrahumidity5), 0) AS extrahumidity5_count,
    min(extrahumidity5) AS extrahumidity5_min,
    max(extrahumidity5) AS extrahumidity5_max,
    -- Extra humidity 6 [extra_humidity6]
    sum(extrahumidity6::double precision) AS extrahumidity6_sum,
    nullif(count(extrahumidity6), 0) AS extrahumidity6_count,
    min(extrahumidity6) AS extrahumidity6_min,
    max(extrahumidity6) AS extrahumidity6_max,
    -- Extra humidity 7 [extra_humidity7]
    sum(extrahumidity7::double precision) AS extrahumidity7_sum,
    nullif(count(extrahumidity7), 0) AS extrahumidity7_count,
    min(extrahumidity7) AS extrahumidity7_min,
    max(extrahumidity7) AS extrahumidity7_max,
    -- Rain rate [rain_rate]
    sum(rainrate::double precision) AS rainrate_sum,
    nullif(count(rainrate), 0) AS rainrate_count,
    max(rainrate) AS rainrate_max,
    -- Rainfall since last reading [rain_incremental]
    sum(rainincremental::double precision) AS rainincremental_total,
    -- Storm rainfall [storm_rain]
    max(stormrain) AS stormrain_max,
    -- Rainfall today [day_rain]
    max(dayrain) AS dayrain_max,
    -- Rainfall this month [month_rain]
    max(monthrain) AS monthrain_max,
    -- Rainfall this year [year_rain]
    max(yearrain) AS yearrain_max,
    -- Solar radiation [solar_watts]
    sum(solarwatts::double precision) AS solarwatts_sum,
    nullif(count(solarwatts), 0) AS solarwatts_count,
    min(solarwatts) AS solarwatts_min,
    max(solarwatts) AS solarwatts_max,
    -- Clear-sky solar radiation [potential_solar_watts]
    sum(potentialsolarwatts::double precision) AS potentialsolarwatts_sum,
    nullif(count(potentialsolarwatts), 0) AS potentialsolarwatts_count,
    min(potentialsolarwatts) AS potentialsolarwatts_min,
    max(potentialsolarwatts) AS potentialsolarwatts_max,
    -- Solar energy [solar_joules]
    sum(solarjoules::double precision) AS solarjoules_sum,
    nullif(count(solarjoules), 0) AS solarjoules_count,
    min(solarjoules) AS solarjoules_min,
    max(solarjoules) AS solarjoules_max,
    -- UV index [uv]
    sum(uv::double precision) AS uv_sum,
    nullif(count(uv), 0) AS uv_count,
    min(uv) AS uv_min,
    max(uv) AS uv_max,
    -- Solar radiation (sensor) [radiation]
    sum(radiation::double precision) AS radiation_sum,
    nullif(count(radiation), 0) AS radiation_count,
    min(radiation) AS radiation_min,
    max(radiation) AS radiation_max,
    -- Evapotranspiration today [day_et]
    max(dayet) AS dayet_max,
    -- Evapotranspiration this month [month_et]
    max(monthet) AS monthet_max,
    -- Evapotranspiration this year [year_et]
    max(yearet) AS yearet_max,
    -- Soil moisture 1 [soil_moisture1]
    sum(soilmoisture1::double precision) AS soilmoisture1_sum,
    nullif(count(soilmoisture1), 0) AS soilmoisture1_count,
    min(soilmoisture1) AS soilmoisture1_min,
    max(soilmoisture1) AS soilmoisture1_max,
    -- Soil moisture 2 [soil_moisture2]
    sum(soilmoisture2::double precision) AS soilmoisture2_sum,
    nullif(count(soilmoisture2), 0) AS soilmoisture2_count,
    min(soilmoisture2) AS soilmoisture2_min,
    max(soilmoisture2) AS soilmoisture2_max,
    -- Soil moisture 3 [soil_moisture3]
    sum(soilmoisture3::double precision) AS soilmoisture3_sum,
    nullif(count(soilmoisture3), 0) AS soilmoisture3_count,
    min(soilmoisture3) AS soilmoisture3_min,
    max(soilmoisture3) AS soilmoisture3_max,
    -- Soil moisture 4 [soil_moisture4]
    sum(soilmoisture4::double precision) AS soilmoisture4_sum,
    nullif(count(soilmoisture4), 0) AS soilmoisture4_count,
    min(soilmoisture4) AS soilmoisture4_min,
    max(soilmoisture4) AS soilmoisture4_max,
    -- Leaf wetness 1 [leaf_wetness1]
    sum(leafwetness1::double precision) AS leafwetness1_sum,
    nullif(count(leafwetness1), 0) AS leafwetness1_count,
    min(leafwetness1) AS leafwetness1_min,
    max(leafwetness1) AS leafwetness1_max,
    -- Leaf wetness 2 [leaf_wetness2]
    sum(leafwetness2::double precision) AS leafwetness2_sum,
    nullif(count(leafwetness2), 0) AS leafwetness2_count,
    min(leafwetness2) AS leafwetness2_min,
    max(leafwetness2) AS leafwetness2_max,
    -- Leaf wetness 3 [leaf_wetness3]
    sum(leafwetness3::double precision) AS leafwetness3_sum,
    nullif(count(leafwetness3), 0) AS leafwetness3_count,
    min(leafwetness3) AS leafwetness3_min,
    max(leafwetness3) AS leafwetness3_max,
    -- Leaf wetness 4 [leaf_wetness4]
    sum(leafwetness4::double precision) AS leafwetness4_sum,
    nullif(count(leafwetness4), 0) AS leafwetness4_count,
    min(leafwetness4) AS leafwetness4_min,
    max(leafwetness4) AS leafwetness4_max,
    -- Indoor alarm [inside_alarm]
    min(insidealarm) AS insidealarm_min,
    max(insidealarm) AS insidealarm_max,
    -- Rain alarm [rain_alarm]
    min(rainalarm) AS rainalarm_min,
    max(rainalarm) AS rainalarm_max,
    -- Outside alarm 1 [outside_alarm1]
    min(outsidealarm1) AS outsidealarm1_min,
    max(outsidealarm1) AS outsidealarm1_max,
    -- Outside alarm 2 [outside_alarm2]
    min(outsidealarm2) AS outsidealarm2_min,
    max(outsidealarm2) AS outsidealarm2_max,
    -- Extra alarm 1 [extra_alarm1]
    min(extraalarm1) AS extraalarm1_min,
    max(extraalarm1) AS extraalarm1_max,
    -- Extra alarm 2 [extra_alarm2]
    min(extraalarm2) AS extraalarm2_min,
    max(extraalarm2) AS extraalarm2_max,
    -- Extra alarm 3 [extra_alarm3]
    min(extraalarm3) AS extraalarm3_min,
    max(extraalarm3) AS extraalarm3_max,
    -- Extra alarm 4 [extra_alarm4]
    min(extraalarm4) AS extraalarm4_min,
    max(extraalarm4) AS extraalarm4_max,
    -- Extra alarm 5 [extra_alarm5]
    min(extraalarm5) AS extraalarm5_min,
    max(extraalarm5) AS extraalarm5_max,
    -- Extra alarm 6 [extra_alarm6]
    min(extraalarm6) AS extraalarm6_min,
    max(extraalarm6) AS extraalarm6_max,
    -- Extra alarm 7 [extra_alarm7]
    min(extraalarm7) AS extraalarm7_min,
    max(extraalarm7) AS extraalarm7_max,
    -- Extra alarm 8 [extra_alarm8]
    min(extraalarm8) AS extraalarm8_min,
    max(extraalarm8) AS extraalarm8_max,
    -- Soil/leaf alarm 1 [soil_leaf_alarm1]
    min(soilleafalarm1) AS soilleafalarm1_min,
    max(soilleafalarm1) AS soilleafalarm1_max,
    -- Soil/leaf alarm 2 [soil_leaf_alarm2]
    min(soilleafalarm2) AS soilleafalarm2_min,
    max(soilleafalarm2) AS soilleafalarm2_max,
    -- Soil/leaf alarm 3 [soil_leaf_alarm3]
    min(soilleafalarm3) AS soilleafalarm3_min,
    max(soilleafalarm3) AS soilleafalarm3_max,
    -- Soil/leaf alarm 4 [soil_leaf_alarm4]
    min(soilleafalarm4) AS soilleafalarm4_min,
    max(soilleafalarm4) AS soilleafalarm4_max,
    -- Transmitter battery [tx_battery_status]
    min(txbatterystatus) AS txbatterystatus_min,
    max(txbatterystatus) AS txbatterystatus_max,
    -- Console battery voltage [cons_battery_voltage]
    sum(consbatteryvoltage::double precision) AS consbatteryvoltage_sum,
    nullif(count(consbatteryvoltage), 0) AS consbatteryvoltage_count,
    min(consbatteryvoltage) AS consbatteryvoltage_min,
    max(consbatteryvoltage) AS consbatteryvoltage_max,
    -- Station battery voltage [station_battery_voltage]
    sum(stationbatteryvoltage::double precision) AS stationbatteryvoltage_sum,
    nullif(count(stationbatteryvoltage), 0) AS stationbatteryvoltage_count,
    min(stationbatteryvoltage) AS stationbatteryvoltage_min,
    max(stationbatteryvoltage) AS stationbatteryvoltage_max,
    -- Snow sensor distance [snow_distance]
    sum(snowdistance::double precision) AS snowdistance_sum,
    nullif(count(snowdistance), 0) AS snowdistance_count,
    min(snowdistance) AS snowdistance_min,
    max(snowdistance) AS snowdistance_max,
    -- Snow depth [snow_depth]
    sum(snowdepth::double precision) AS snowdepth_sum,
    nullif(count(snowdepth), 0) AS snowdepth_count,
    min(snowdepth) AS snowdepth_min,
    max(snowdepth) AS snowdepth_max,
    -- Extra value 1 [extra_float1]
    sum(extrafloat1::double precision) AS extrafloat1_sum,
    nullif(count(extrafloat1), 0) AS extrafloat1_count,
    min(extrafloat1) AS extrafloat1_min,
    max(extrafloat1) AS extrafloat1_max,
    -- Extra value 2 [extra_float2]
    sum(extrafloat2::double precision) AS extrafloat2_sum,
    nullif(count(extrafloat2), 0) AS extrafloat2_count,
    min(extrafloat2) AS extrafloat2_min,
    max(extrafloat2) AS extrafloat2_max,
    -- Extra value 3 [extra_float3]
    sum(extrafloat3::double precision) AS extrafloat3_sum,
    nullif(count(extrafloat3), 0) AS extrafloat3_count,
    min(extrafloat3) AS extrafloat3_min,
    max(extrafloat3) AS extrafloat3_max,
    -- Extra value 4 [extra_float4]
    sum(extrafloat4::double precision) AS extrafloat4_sum,
    nullif(count(extrafloat4), 0) AS extrafloat4_count,
    min(extrafloat4) AS extrafloat4_min,
    max(extrafloat4) AS extrafloat4_max,
    -- Extra value 5 [extra_float5]
    sum(extrafloat5::double precision) AS extrafloat5_sum,
    nullif(count(extrafloat5), 0) AS extrafloat5_count,
    min(extrafloat5) AS extrafloat5_min,
    max(extrafloat5) AS extrafloat5_max,
    -- Extra value 6 [extra_float6]
    sum(extrafloat6::double precision) AS extrafloat6_sum,
    nullif(count(extrafloat6), 0) AS extrafloat6_count,
    min(extrafloat6) AS extrafloat6_min,
    max(extrafloat6) AS extrafloat6_max,
    -- Extra value 7 [extra_float7]
    sum(extrafloat7::double precision) AS extrafloat7_sum,
    nullif(count(extrafloat7), 0) AS extrafloat7_count,
    min(extrafloat7) AS extrafloat7_min,
    max(extrafloat7) AS extrafloat7_max,
    -- Extra value 8 [extra_float8]
    sum(extrafloat8::double precision) AS extrafloat8_sum,
    nullif(count(extrafloat8), 0) AS extrafloat8_count,
    min(extrafloat8) AS extrafloat8_min,
    max(extrafloat8) AS extrafloat8_max,
    -- Extra value 9 [extra_float9]
    sum(extrafloat9::double precision) AS extrafloat9_sum,
    nullif(count(extrafloat9), 0) AS extrafloat9_count,
    min(extrafloat9) AS extrafloat9_min,
    max(extrafloat9) AS extrafloat9_max,
    -- Extra value 10 [extra_float10]
    sum(extrafloat10::double precision) AS extrafloat10_sum,
    nullif(count(extrafloat10), 0) AS extrafloat10_count,
    min(extrafloat10) AS extrafloat10_min,
    max(extrafloat10) AS extrafloat10_max,
    -- Temperature sensor 1 [temp1]
    sum(temp1::double precision) AS temp1_sum,
    nullif(count(temp1), 0) AS temp1_count,
    min(temp1) AS temp1_min,
    max(temp1) AS temp1_max,
    -- Temperature sensor 2 [temp2]
    sum(temp2::double precision) AS temp2_sum,
    nullif(count(temp2), 0) AS temp2_count,
    min(temp2) AS temp2_min,
    max(temp2) AS temp2_max,
    -- Temperature sensor 3 [temp3]
    sum(temp3::double precision) AS temp3_sum,
    nullif(count(temp3), 0) AS temp3_count,
    min(temp3) AS temp3_min,
    max(temp3) AS temp3_max,
    -- Temperature sensor 4 [temp4]
    sum(temp4::double precision) AS temp4_sum,
    nullif(count(temp4), 0) AS temp4_count,
    min(temp4) AS temp4_min,
    max(temp4) AS temp4_max,
    -- Temperature sensor 5 [temp5]
    sum(temp5::double precision) AS temp5_sum,
    nullif(count(temp5), 0) AS temp5_count,
    min(temp5) AS temp5_min,
    max(temp5) AS temp5_max,
    -- Temperature sensor 6 [temp6]
    sum(temp6::double precision) AS temp6_sum,
    nullif(count(temp6), 0) AS temp6_count,
    min(temp6) AS temp6_min,
    max(temp6) AS temp6_max,
    -- Temperature sensor 7 [temp7]
    sum(temp7::double precision) AS temp7_sum,
    nullif(count(temp7), 0) AS temp7_count,
    min(temp7) AS temp7_min,
    max(temp7) AS temp7_max,
    -- Temperature sensor 8 [temp8]
    sum(temp8::double precision) AS temp8_sum,
    nullif(count(temp8), 0) AS temp8_count,
    min(temp8) AS temp8_min,
    max(temp8) AS temp8_max,
    -- Temperature sensor 9 [temp9]
    sum(temp9::double precision) AS temp9_sum,
    nullif(count(temp9), 0) AS temp9_count,
    min(temp9) AS temp9_min,
    max(temp9) AS temp9_max,
    -- Temperature sensor 10 [temp10]
    sum(temp10::double precision) AS temp10_sum,
    nullif(count(temp10), 0) AS temp10_count,
    min(temp10) AS temp10_min,
    max(temp10) AS temp10_max,
    -- Soil temperature 5 [soil_temp5]
    sum(soiltemp5::double precision) AS soiltemp5_sum,
    nullif(count(soiltemp5), 0) AS soiltemp5_count,
    min(soiltemp5) AS soiltemp5_min,
    max(soiltemp5) AS soiltemp5_max,
    -- Soil temperature 6 [soil_temp6]
    sum(soiltemp6::double precision) AS soiltemp6_sum,
    nullif(count(soiltemp6), 0) AS soiltemp6_count,
    min(soiltemp6) AS soiltemp6_min,
    max(soiltemp6) AS soiltemp6_max,
    -- Soil temperature 7 [soil_temp7]
    sum(soiltemp7::double precision) AS soiltemp7_sum,
    nullif(count(soiltemp7), 0) AS soiltemp7_count,
    min(soiltemp7) AS soiltemp7_min,
    max(soiltemp7) AS soiltemp7_max,
    -- Soil temperature 8 [soil_temp8]
    sum(soiltemp8::double precision) AS soiltemp8_sum,
    nullif(count(soiltemp8), 0) AS soiltemp8_count,
    min(soiltemp8) AS soiltemp8_min,
    max(soiltemp8) AS soiltemp8_max,
    -- Soil temperature 9 [soil_temp9]
    sum(soiltemp9::double precision) AS soiltemp9_sum,
    nullif(count(soiltemp9), 0) AS soiltemp9_count,
    min(soiltemp9) AS soiltemp9_min,
    max(soiltemp9) AS soiltemp9_max,
    -- Soil temperature 10 [soil_temp10]
    sum(soiltemp10::double precision) AS soiltemp10_sum,
    nullif(count(soiltemp10), 0) AS soiltemp10_count,
    min(soiltemp10) AS soiltemp10_min,
    max(soiltemp10) AS soiltemp10_max,
    -- Humidity sensor 1 [humidity1]
    sum(humidity1::double precision) AS humidity1_sum,
    nullif(count(humidity1), 0) AS humidity1_count,
    min(humidity1) AS humidity1_min,
    max(humidity1) AS humidity1_max,
    -- Humidity sensor 2 [humidity2]
    sum(humidity2::double precision) AS humidity2_sum,
    nullif(count(humidity2), 0) AS humidity2_count,
    min(humidity2) AS humidity2_min,
    max(humidity2) AS humidity2_max,
    -- Humidity sensor 3 [humidity3]
    sum(humidity3::double precision) AS humidity3_sum,
    nullif(count(humidity3), 0) AS humidity3_count,
    min(humidity3) AS humidity3_min,
    max(humidity3) AS humidity3_max,
    -- Humidity sensor 4 [humidity4]
    sum(humidity4::double precision) AS humidity4_sum,
    nullif(count(humidity4), 0) AS humidity4_count,
    min(humidity4) AS humidity4_min,
    max(humidity4) AS humidity4_max,
    -- Humidity sensor 5 [humidity5]
    sum(humidity5::double precision) AS humidity5_sum,
    nullif(count(humidity5), 0) AS humidity5_count,
    min(humidity5) AS humidity5_min,
    max(humidity5) AS humidity5_max,
    -- Humidity sensor 6 [humidity6]
    sum(humidity6::double precision) AS humidity6_sum,
    nullif(count(humidity6), 0) AS humidity6_count,
    min(humidity6) AS humidity6_min,
    max(humidity6) AS humidity6_max,
    -- Humidity sensor 7 [humidity7]
    sum(humidity7::double precision) AS humidity7_sum,
    nullif(count(humidity7), 0) AS humidity7_count,
    min(humidity7) AS humidity7_min,
    max(humidity7) AS humidity7_max,
    -- Humidity sensor 8 [humidity8]
    sum(humidity8::double precision) AS humidity8_sum,
    nullif(count(humidity8), 0) AS humidity8_count,
    min(humidity8) AS humidity8_min,
    max(humidity8) AS humidity8_max,
    -- Humidity sensor 9 [humidity9]
    sum(humidity9::double precision) AS humidity9_sum,
    nullif(count(humidity9), 0) AS humidity9_count,
    min(humidity9) AS humidity9_min,
    max(humidity9) AS humidity9_max,
    -- Humidity sensor 10 [humidity10]
    sum(humidity10::double precision) AS humidity10_sum,
    nullif(count(humidity10), 0) AS humidity10_count,
    min(humidity10) AS humidity10_min,
    max(humidity10) AS humidity10_max,
    -- Soil humidity 1 [soil_hum1]
    sum(soilhum1::double precision) AS soilhum1_sum,
    nullif(count(soilhum1), 0) AS soilhum1_count,
    min(soilhum1) AS soilhum1_min,
    max(soilhum1) AS soilhum1_max,
    -- Soil humidity 2 [soil_hum2]
    sum(soilhum2::double precision) AS soilhum2_sum,
    nullif(count(soilhum2), 0) AS soilhum2_count,
    min(soilhum2) AS soilhum2_min,
    max(soilhum2) AS soilhum2_max,
    -- Soil humidity 3 [soil_hum3]
    sum(soilhum3::double precision) AS soilhum3_sum,
    nullif(count(soilhum3), 0) AS soilhum3_count,
    min(soilhum3) AS soilhum3_min,
    max(soilhum3) AS soilhum3_max,
    -- Soil humidity 4 [soil_hum4]
    sum(soilhum4::double precision) AS soilhum4_sum,
    nullif(count(soilhum4), 0) AS soilhum4_count,
    min(soilhum4) AS soilhum4_min,
    max(soilhum4) AS soilhum4_max,
    -- Soil humidity 5 [soil_hum5]
    sum(soilhum5::double precision) AS soilhum5_sum,
    nullif(count(soilhum5), 0) AS soilhum5_count,
    min(soilhum5) AS soilhum5_min,
    max(soilhum5) AS soilhum5_max,
    -- Soil humidity 6 [soil_hum6]
    sum(soilhum6::double precision) AS soilhum6_sum,
    nullif(count(soilhum6), 0) AS soilhum6_count,
    min(soilhum6) AS soilhum6_min,
    max(soilhum6) AS soilhum6_max,
    -- Soil humidity 7 [soil_hum7]
    sum(soilhum7::double precision) AS soilhum7_sum,
    nullif(count(soilhum7), 0) AS soilhum7_count,
    min(soilhum7) AS soilhum7_min,
    max(soilhum7) AS soilhum7_max,
    -- Soil humidity 8 [soil_hum8]
    sum(soilhum8::double precision) AS soilhum8_sum,
    nullif(count(soilhum8), 0) AS soilhum8_count,
    min(soilhum8) AS soilhum8_min,
    max(soilhum8) AS soilhum8_max,
    -- Soil humidity 9 [soil_hum9]
    sum(soilhum9::double precision) AS soilhum9_sum,
    nullif(count(soilhum9), 0) AS soilhum9_count,
    min(soilhum9) AS soilhum9_min,
    max(soilhum9) AS soilhum9_max,
    -- Soil humidity 10 [soil_hum10]
    sum(soilhum10::double precision) AS soilhum10_sum,
    nullif(count(soilhum10), 0) AS soilhum10_count,
    min(soilhum10) AS soilhum10_min,
    max(soilhum10) AS soilhum10_max,
    -- Leaf wetness 5 [leaf_wetness5]
    sum(leafwetness5::double precision) AS leafwetness5_sum,
    nullif(count(leafwetness5), 0) AS leafwetness5_count,
    min(leafwetness5) AS leafwetness5_min,
    max(leafwetness5) AS leafwetness5_max,
    -- Leaf wetness 6 [leaf_wetness6]
    sum(leafwetness6::double precision) AS leafwetness6_sum,
    nullif(count(leafwetness6), 0) AS leafwetness6_count,
    min(leafwetness6) AS leafwetness6_min,
    max(leafwetness6) AS leafwetness6_max,
    -- Leaf wetness 7 [leaf_wetness7]
    sum(leafwetness7::double precision) AS leafwetness7_sum,
    nullif(count(leafwetness7), 0) AS leafwetness7_count,
    min(leafwetness7) AS leafwetness7_min,
    max(leafwetness7) AS leafwetness7_max,
    -- Leaf wetness 8 [leaf_wetness8]
    sum(leafwetness8::double precision) AS leafwetness8_sum,
    nullif(count(leafwetness8), 0) AS leafwetness8_count,
    min(leafwetness8) AS leafwetness8_min,
    max(leafwetness8) AS leafwetness8_max,
    -- Soil tension 1 [soil_tens1]
    sum(soiltens1::double precision) AS soiltens1_sum,
    nullif(count(soiltens1), 0) AS soiltens1_count,
    min(soiltens1) AS soiltens1_min,
    max(soiltens1) AS soiltens1_max,
    -- Soil tension 2 [soil_tens2]
    sum(soiltens2::double precision) AS soiltens2_sum,
    nullif(count(soiltens2), 0) AS soiltens2_count,
    min(soiltens2) AS soiltens2_min,
    max(soiltens2) AS soiltens2_max,
    -- Soil tension 3 [soil_tens3]
    sum(soiltens3::double precision) AS soiltens3_sum,
    nullif(count(soiltens3), 0) AS soiltens3_count,
    min(soiltens3) AS soiltens3_min,
    max(soiltens3) AS soiltens3_max,
    -- Soil tension 4 [soil_tens4]
    sum(soiltens4::double precision) AS soiltens4_sum,
    nullif(count(soiltens4), 0) AS soiltens4_count,
    min(soiltens4) AS soiltens4_min,
    max(soiltens4) AS soiltens4_max,
    -- Growing degree days [gdd]
    max(gdd) AS gdd_max,
    -- Reference evapotranspiration (short crop) [etos]
    sum(etos::double precision) AS etos_sum,
    nullif(count(etos), 0) AS etos_count,
    min(etos) AS etos_min,
    max(etos) AS etos_max,
    -- Reference evapotranspiration (tall crop) [etrs]
    sum(etrs::double precision) AS etrs_sum,
    nullif(count(etrs), 0) AS etrs_count,
    min(etrs) AS etrs_min,
    max(etrs) AS etrs_max,
    -- Leak detector 1 [leak1]
    min(leak1) AS leak1_min,
    max(leak1) AS leak1_max,
    -- Leak detector 2 [leak2]
    min(leak2) AS leak2_min,
    max(leak2) AS leak2_max,
    -- Leak detector 3 [leak3]
    min(leak3) AS leak3_min,
    max(leak3) AS leak3_max,
    -- Leak detector 4 [leak4]
    min(leak4) AS leak4_min,
    max(leak4) AS leak4_max,
    -- Outdoor sensor battery [batt_out]
    min(battout) AS battout_min,
    max(battout) AS battout_max,
    -- Indoor sensor battery [batt_in]
    min(battin) AS battin_min,
    max(battin) AS battin_max,
    -- Battery 1 [batt1]
    min(batt1) AS batt1_min,
    max(batt1) AS batt1_max,
    -- Battery 2 [batt2]
    min(batt2) AS batt2_min,
    max(batt2) AS batt2_max,
    -- Battery 3 [batt3]
    min(batt3) AS batt3_min,
    max(batt3) AS batt3_max,
    -- Battery 4 [batt4]
    min(batt4) AS batt4_min,
    max(batt4) AS batt4_max,
    -- Battery 5 [batt5]
    min(batt5) AS batt5_min,
    max(batt5) AS batt5_max,
    -- Battery 6 [batt6]
    min(batt6) AS batt6_min,
    max(batt6) AS batt6_max,
    -- Battery 7 [batt7]
    min(batt7) AS batt7_min,
    max(batt7) AS batt7_max,
    -- Battery 8 [batt8]
    min(batt8) AS batt8_min,
    max(batt8) AS batt8_max,
    -- Battery 9 [batt9]
    min(batt9) AS batt9_min,
    max(batt9) AS batt9_max,
    -- Battery 10 [batt10]
    min(batt10) AS batt10_min,
    max(batt10) AS batt10_max,
    -- Particulate sensor battery [batt_25]
    min(batt_25) AS batt_25_min,
    max(batt_25) AS batt_25_max,
    -- Lightning sensor battery [batt_lightning]
    min(batt_lightning) AS batt_lightning_min,
    max(batt_lightning) AS batt_lightning_max,
    -- Leak detector 1 battery [bat_leak1]
    min(batleak1) AS batleak1_min,
    max(batleak1) AS batleak1_max,
    -- Leak detector 2 battery [bat_leak2]
    min(batleak2) AS batleak2_min,
    max(batleak2) AS batleak2_max,
    -- Leak detector 3 battery [bat_leak3]
    min(batleak3) AS batleak3_min,
    max(batleak3) AS batleak3_max,
    -- Leak detector 4 battery [bat_leak4]
    min(batleak4) AS batleak4_min,
    max(batleak4) AS batleak4_max,
    -- Soil moisture sensor 1 battery [batt_sm1]
    min(battsm1) AS battsm1_min,
    max(battsm1) AS battsm1_max,
    -- Soil moisture sensor 2 battery [batt_sm2]
    min(battsm2) AS battsm2_min,
    max(battsm2) AS battsm2_max,
    -- Soil moisture sensor 3 battery [batt_sm3]
    min(battsm3) AS battsm3_min,
    max(battsm3) AS battsm3_max,
    -- Soil moisture sensor 4 battery [batt_sm4]
    min(battsm4) AS battsm4_min,
    max(battsm4) AS battsm4_max,
    -- CO2 sensor battery [batt_co2]
    min(batt_co2) AS batt_co2_min,
    max(batt_co2) AS batt_co2_max,
    -- Cellular gateway battery [batt_cellgateway]
    min(batt_cellgateway) AS batt_cellgateway_min,
    max(batt_cellgateway) AS batt_cellgateway_max,
    -- Pressure (relative) [barom_rel_in]
    sum(baromrelin::double precision) AS baromrelin_sum,
    nullif(count(baromrelin), 0) AS baromrelin_count,
    min(baromrelin) AS baromrelin_min,
    max(baromrelin) AS baromrelin_max,
    -- Pressure (absolute) [barom_abs_in]
    sum(baromabsin::double precision) AS baromabsin_sum,
    nullif(count(baromabsin), 0) AS baromabsin_count,
    min(baromabsin) AS baromabsin_min,
    max(baromabsin) AS baromabsin_max,
    -- Relay 1 [relay1]
    min(relay1) AS relay1_min,
    max(relay1) AS relay1_max,
    -- Relay 2 [relay2]
    min(relay2) AS relay2_min,
    max(relay2) AS relay2_max,
    -- Relay 3 [relay3]
    min(relay3) AS relay3_min,
    max(relay3) AS relay3_max,
    -- Relay 4 [relay4]
    min(relay4) AS relay4_min,
    max(relay4) AS relay4_max,
    -- Relay 5 [relay5]
    min(relay5) AS relay5_min,
    max(relay5) AS relay5_max,
    -- Relay 6 [relay6]
    min(relay6) AS relay6_min,
    max(relay6) AS relay6_max,
    -- Relay 7 [relay7]
    min(relay7) AS relay7_min,
    max(relay7) AS relay7_max,
    -- Relay 8 [relay8]
    min(relay8) AS relay8_min,
    max(relay8) AS relay8_max,
    -- Relay 9 [relay9]
    min(relay9) AS relay9_min,
    max(relay9) AS relay9_max,
    -- Relay 10 [relay10]
    min(relay10) AS relay10_min,
    max(relay10) AS relay10_max,
    -- PM2.5 [pm25]
    sum(pm25::double precision) AS pm25_sum,
    nullif(count(pm25), 0) AS pm25_count,
    min(pm25) AS pm25_min,
    max(pm25) AS pm25_max,
    -- PM2.5 (24-hour average) [pm25_24h]
    sum(pm25_24h::double precision) AS pm25_24h_sum,
    nullif(count(pm25_24h), 0) AS pm25_24h_count,
    min(pm25_24h) AS pm25_24h_min,
    max(pm25_24h) AS pm25_24h_max,
    -- PM2.5 indoors [pm25_in]
    sum(pm25_in::double precision) AS pm25_in_sum,
    nullif(count(pm25_in), 0) AS pm25_in_count,
    min(pm25_in) AS pm25_in_min,
    max(pm25_in) AS pm25_in_max,
    -- PM2.5 indoors (24-hour average) [pm25_in_24h]
    sum(pm25_in_24h::double precision) AS pm25_in_24h_sum,
    nullif(count(pm25_in_24h), 0) AS pm25_in_24h_count,
    min(pm25_in_24h) AS pm25_in_24h_min,
    max(pm25_in_24h) AS pm25_in_24h_max,
    -- PM2.5 indoors (AQIN sensor) [pm25_in_aqin]
    sum(pm25_in_aqin::double precision) AS pm25_in_aqin_sum,
    nullif(count(pm25_in_aqin), 0) AS pm25_in_aqin_count,
    min(pm25_in_aqin) AS pm25_in_aqin_min,
    max(pm25_in_aqin) AS pm25_in_aqin_max,
    -- PM2.5 indoors, 24-hour average (AQIN sensor) [pm25_in_24h_aqin]
    sum(pm25_in_24h_aqin::double precision) AS pm25_in_24h_aqin_sum,
    nullif(count(pm25_in_24h_aqin), 0) AS pm25_in_24h_aqin_count,
    min(pm25_in_24h_aqin) AS pm25_in_24h_aqin_min,
    max(pm25_in_24h_aqin) AS pm25_in_24h_aqin_max,
    -- PM10 indoors (AQIN sensor) [pm10_in_aqin]
    sum(pm10_in_aqin::double precision) AS pm10_in_aqin_sum,
    nullif(count(pm10_in_aqin), 0) AS pm10_in_aqin_count,
    min(pm10_in_aqin) AS pm10_in_aqin_min,
    max(pm10_in_aqin) AS pm10_in_aqin_max,
    -- PM10 indoors, 24-hour average (AQIN sensor) [pm10_in_24h_aqin]
    sum(pm10_in_24h_aqin::double precision) AS pm10_in_24h_aqin_sum,
    nullif(count(pm10_in_24h_aqin), 0) AS pm10_in_24h_aqin_count,
    min(pm10_in_24h_aqin) AS pm10_in_24h_aqin_min,
    max(pm10_in_24h_aqin) AS pm10_in_24h_aqin_max,
    -- CO2 [co2]
    sum(co2::double precision) AS co2_sum,
    nullif(count(co2), 0) AS co2_count,
    min(co2) AS co2_min,
    max(co2) AS co2_max,
    -- CO2 indoors (AQIN sensor) [co2_in_aqin]
    sum(co2_in_aqin::double precision) AS co2_in_aqin_sum,
    nullif(count(co2_in_aqin), 0) AS co2_in_aqin_count,
    min(co2_in_aqin) AS co2_in_aqin_min,
    max(co2_in_aqin) AS co2_in_aqin_max,
    -- CO2 indoors, 24-hour average (AQIN sensor) [co2_in_24h_aqin]
    sum(co2_in_24h_aqin::double precision) AS co2_in_24h_aqin_sum,
    nullif(count(co2_in_24h_aqin), 0) AS co2_in_24h_aqin_count,
    min(co2_in_24h_aqin) AS co2_in_24h_aqin_min,
    max(co2_in_24h_aqin) AS co2_in_24h_aqin_max,
    -- Indoor temperature (AQIN sensor) [pm_in_temp_aqin]
    sum(pm_in_temp_aqin::double precision) AS pm_in_temp_aqin_sum,
    nullif(count(pm_in_temp_aqin), 0) AS pm_in_temp_aqin_count,
    min(pm_in_temp_aqin) AS pm_in_temp_aqin_min,
    max(pm_in_temp_aqin) AS pm_in_temp_aqin_max,
    -- Indoor humidity (AQIN sensor) [pm_in_humidity_aqin]
    sum(pm_in_humidity_aqin::double precision) AS pm_in_humidity_aqin_sum,
    nullif(count(pm_in_humidity_aqin), 0) AS pm_in_humidity_aqin_count,
    min(pm_in_humidity_aqin) AS pm_in_humidity_aqin_min,
    max(pm_in_humidity_aqin) AS pm_in_humidity_aqin_max,
    -- Air quality index, PM2.5 (AQIN sensor) [aqi_pm25_aqin]
    sum(aqi_pm25_aqin::double precision) AS aqi_pm25_aqin_sum,
    nullif(count(aqi_pm25_aqin), 0) AS aqi_pm25_aqin_count,
    min(aqi_pm25_aqin) AS aqi_pm25_aqin_min,
    max(aqi_pm25_aqin) AS aqi_pm25_aqin_max,
    -- Air quality index, PM2.5 24-hour (AQIN sensor) [aqi_pm25_24h_aqin]
    sum(aqi_pm25_24h_aqin::double precision) AS aqi_pm25_24h_aqin_sum,
    nullif(count(aqi_pm25_24h_aqin), 0) AS aqi_pm25_24h_aqin_count,
    min(aqi_pm25_24h_aqin) AS aqi_pm25_24h_aqin_min,
    max(aqi_pm25_24h_aqin) AS aqi_pm25_24h_aqin_max,
    -- Air quality index, PM10 (AQIN sensor) [aqi_pm10_aqin]
    sum(aqi_pm10_aqin::double precision) AS aqi_pm10_aqin_sum,
    nullif(count(aqi_pm10_aqin), 0) AS aqi_pm10_aqin_count,
    min(aqi_pm10_aqin) AS aqi_pm10_aqin_min,
    max(aqi_pm10_aqin) AS aqi_pm10_aqin_max,
    -- Air quality index, PM10 24-hour (AQIN sensor) [aqi_pm10_24h_aqin]
    sum(aqi_pm10_24h_aqin::double precision) AS aqi_pm10_24h_aqin_sum,
    nullif(count(aqi_pm10_24h_aqin), 0) AS aqi_pm10_24h_aqin_count,
    min(aqi_pm10_24h_aqin) AS aqi_pm10_24h_aqin_min,
    max(aqi_pm10_24h_aqin) AS aqi_pm10_24h_aqin_max,
    -- Air quality index, PM2.5 indoors [aqi_pm25_in]
    sum(aqi_pm25_in::double precision) AS aqi_pm25_in_sum,
    nullif(count(aqi_pm25_in), 0) AS aqi_pm25_in_count,
    min(aqi_pm25_in) AS aqi_pm25_in_min,
    max(aqi_pm25_in) AS aqi_pm25_in_max,
    -- Air quality index, PM2.5 indoors 24-hour [aqi_pm25_in_24h]
    sum(aqi_pm25_in_24h::double precision) AS aqi_pm25_in_24h_sum,
    nullif(count(aqi_pm25_in_24h), 0) AS aqi_pm25_in_24h_count,
    min(aqi_pm25_in_24h) AS aqi_pm25_in_24h_min,
    max(aqi_pm25_in_24h) AS aqi_pm25_in_24h_max,
    -- Lightning strikes today [lightning_day]
    max(lightning_day) AS lightning_day_max,
    -- Lightning strikes this hour [lightning_hour]
    max(lightning_hour) AS lightning_hour_max,
    -- Distance to last lightning strike [lightning_distance]
    sum(lightning_distance::double precision) AS lightning_distance_sum,
    nullif(count(lightning_distance), 0) AS lightning_distance_count,
    min(lightning_distance) AS lightning_distance_min,
    max(lightning_distance) AS lightning_distance_max
FROM observations
GROUP BY 1, 2
;
\timing off
SELECT pg_size_pretty(pg_total_relation_size('mw_measure.plain_1m')) AS plain_table_size,
       count(*) AS plain_rows FROM mw_measure.plain_1m;
DROP TABLE mw_measure.plain_1m;
