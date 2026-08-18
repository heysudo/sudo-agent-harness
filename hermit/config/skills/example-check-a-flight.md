# Check whether a flight is delayed
goal: find the current status of a specific flight by number

steps:
1. web_search the flight number plus today's date and the word "status"
2. read the excerpt for scheduled vs estimated time; only fetch_page if the excerpt
   gives no estimated time

parameters:
- include the airline code with the number ("BA 142", not "142")
- say the delay in minutes, and the new time, in that order

gotchas:
- flight numbers are reused daily; without a date the result may be yesterday's leg
