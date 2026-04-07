module consumer_values
    use relay_values, only: lifted
    implicit none

    integer, parameter :: final_value = lifted + lifted
end module consumer_values
