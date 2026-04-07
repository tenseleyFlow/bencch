module layered_delta_metrics
    use layered_left_metrics, only: doubled
    use layered_right_metrics, only: shifted
    implicit none

    integer, parameter :: gap = doubled - shifted
end module layered_delta_metrics
