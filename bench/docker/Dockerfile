# Same static binary as bench/ply/api, on the Docker-side equivalent of
# ply's debian@13 base.
FROM debian:13-slim
COPY server /server
EXPOSE 8080
ENTRYPOINT ["/server"]
