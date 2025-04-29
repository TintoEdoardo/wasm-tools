#!/bin/bash

cpu=/sys/devices/system/cpu/

# Disable hyperthreads.
# This code should be modified, it depend on the CPU topology,
# to be checked with: cat cpu0/topology/core_cpus_list.
# In our case, CPU8 and 4 have no other core in their unit. 
for c in $(seq 8 8); do
  echo 0 > $cpu/cpu$c/online
done
echo 1 > $cpu/cpu8/online


############################
#      Tuning script       #
############################

echo "Tuning starting... "

# Disable turbo-boost.
echo 1 > $cpu/intel_pstate/no_turbo

# Set max freq.
orig_min_perf_pct=$(cat $cpu/intel_pstate/min_perf_pct)
cat $cpu/intel_pstate/max_perf_pct > $cpu/intel_pstate/min_perf_pct

# Scaling policy and adjust frequency.
orig_scaling_governor_4=$(cat $cpu/cpufreq/policy4/scaling_governor)
orig_scaling_min_freq_4=$(cat $cpu/cpufreq/policy4/scaling_min_freq)
orig_scaling_max_freq_4=$(cat $cpu/cpufreq/policy4/scaling_max_freq)

orig_scaling_governor_8=$(cat $cpu/cpufreq/policy8/scaling_governor)
orig_scaling_min_freq_8=$(cat $cpu/cpufreq/policy8/scaling_min_freq)
orig_scaling_max_freq_8=$(cat $cpu/cpufreq/policy8/scaling_max_freq)

echo "performance" > $cpu/cpu8/cpufreq/scaling_governor
cat $cpu/cpu8/cpufreq/base_frequency > $cpu/cpu8/cpufreq/scaling_min_freq
cat $cpu/cpu8/cpufreq/base_frequency > $cpu/cpu8/cpufreq//scaling_max_freq

echo "performance" > $cpu/cpufreq/policy8/scaling_governor
cat $cpu/cpufreq/policy8/base_frequency > $cpu/cpufreq/policy8/scaling_min_freq
cat $cpu/cpufreq/policy8/base_frequency > $cpu/cpufreq/policy8/scaling_max_freq

echo "performance" > $cpu/cpufreq/policy4/scaling_governor
cat $cpu/cpufreq/policy4/base_frequency > $cpu/cpufreq/policy4/scaling_min_freq
cat $cpu/cpufreq/policy4/base_frequency > $cpu/cpufreq/policy4/scaling_max_freq


# Disable deep idle state.
for state in $cpu/cpu8/cpuidle/state*; do
  echo 1 > $state/disable
done

for state in $cpu/cpu4/cpuidle/state*; do
  echo 1 > $state/disable
done

echo "Tuning completed. "

################################
#     Run the experiments      #
################################
 
echo "The experiment begin..."

# echo "First batch: "
# sudo chrt -f 99 ./evaluation 5 false true  > eval_o_1.txt
# sudo chrt -f 99 ./evaluation 5 true false  > eval_d_1.txt

# echo "Second batch: "
# sudo chrt -f 99 ./evaluation 5 false true  > eval_o_2.txt
# sudo chrt -f 99 ./evaluation 5 true false  > eval_d_2.txt
# sudo chrt -f 99 ./evaluation 5 false false > eval_c_1.txt

# echo "Third batch: "
# sudo chrt -f 99 ./evaluation 5 false true  > eval_o_3.txt
# sudo chrt -f 99 ./evaluation 5 true false  > eval_d_3.txt
# sudo chrt -f 99 ./evaluation 5 false false > eval_c_2.txt

# echo "Fourth batch: "
# sudo chrt -f 99 ./evaluation 5 false true  > eval_o_4.txt
# sudo chrt -f 99 ./evaluation 5 true false  > eval_d_4.txt
# sudo chrt -f 99 ./evaluation 5 false false > eval_c_3.txt
# sudo chrt -f 99 ./evaluation 5 false false > eval_c_4.txt

sudo chrt -f 99 ./end_to_end_eval false 6 6 > end_to_end_6_6.txt
sudo chrt -f 99 ./end_to_end_eval false 1 4 > end_to_end_1_4.txt

sudo chrt -f 99 ./memory_transfer false 6 6 > memory_transfer_6_6.txt
sudo chrt -f 99 ./memory_transfer false 1 4 > memory_transfer_1_4.txt

##############################
#      Cleaning script       #
##############################

echo "Cleaning starting..."

echo 0 > $cpu/intel_pstate/no_turbo

echo $orig_min_perf_pct > $cpu/intel_pstate/min_perf_pct
echo $orig_scaling_governor_4 > $cpu/cpufreq/policy4/scaling_governor
echo $orig_scaling_min_freq_4 > $cpu/cpufreq/policy4/scaling_min_freq
echo $orig_scaling_max_freq_4 > $cpu/cpufreq/policy4/scaling_max_freq

echo $orig_scaling_governor_8 > $cpu/cpufreq/policy8/scaling_governor
echo $orig_scaling_min_freq_8 > $cpu/cpufreq/policy8/scaling_min_freq
echo $orig_scaling_max_freq_8 > $cpu/cpufreq/policy8/scaling_max_freq

for state in $cpu/cpu4/cpuidle/state*; do
  echo 0 > $state/disable
done

for state in $cpu/cpu8/cpuidle/state*; do
  echo 0 > $state/disable
done

echo "Cleaning completed. "


