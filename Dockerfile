FROM rust:latest

RUN apt update && apt install -y protobuf-compiler
