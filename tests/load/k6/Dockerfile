FROM grafana/k6:0.54.0
COPY v1-stress.js /scripts/v1-stress.js
ENTRYPOINT ["k6", "run", "/scripts/v1-stress.js"]
